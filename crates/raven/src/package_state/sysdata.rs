//! AST-first extraction of `R/sysdata.rda` symbol names and `.onLoad`/`.onAttach` bindings.
//!
//! Part 1: scans `data-raw/**/*.R` (and any other non-built R files under the
//! workspace root) for generating calls:
//! - `usethis::use_data(..., internal = TRUE)` / `use_data(..., internal = TRUE)` → positional args
//! - `save(..., file = "...sysdata.rda")` / `save(list = c("a","b"), file = "...sysdata.rda")`
//!
//! Part 2: `.onLoad`/`.onAttach` body scanning for namespace-level bindings:
//! - `assign("x", ..., envir = ...)` at top level inside the hook
//! - `ns$x <- ...` / `topenv()$x <- ...` inside the hook
//! - `makeActiveBinding("x", fn, <namespace-env>)` inside the hook

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use tree_sitter::{Node, Parser};

/// Scan `data-raw/**/*.R` (recursively) for `use_data(..., internal=TRUE)` and
/// `save(..., file="...sysdata.rda")` calls. Returns the set of symbol names
/// that would be written to `R/sysdata.rda`.
pub fn scan_sysdata_generating_scripts(workspace_root: &Path) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    let data_raw = workspace_root.join("data-raw");
    if data_raw.is_dir() {
        scan_dir_recursive(&data_raw, &mut symbols);
    }
    symbols
}

/// Like [`scan_sysdata_generating_scripts`], but skips files and prunable
/// directories matched by `[workspace].exclude`.
pub fn scan_sysdata_generating_scripts_with_exclusions(
    workspace_root: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> BTreeSet<String> {
    if exclusions.is_empty() {
        return scan_sysdata_generating_scripts(workspace_root);
    }

    let mut symbols = BTreeSet::new();
    let data_raw = workspace_root.join("data-raw");
    if data_raw.is_dir() && !exclusions.can_prune_directory(&data_raw) {
        scan_dir_recursive_with_exclusions(&data_raw, exclusions, &mut symbols);
    }
    symbols
}

fn scan_dir_recursive(dir: &Path, symbols: &mut BTreeSet<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let path = entry.path();
        if ft.is_dir() && !ft.is_symlink() {
            scan_dir_recursive(&path, symbols);
        } else if (ft.is_file() || (ft.is_symlink() && path.is_file()))
            && matches!(path.extension().and_then(|e| e.to_str()), Some("R" | "r"))
            && let Ok(content) = fs::read_to_string(&path)
        {
            extract_sysdata_names_from_source(&content, symbols);
        }
    }
}

fn scan_dir_recursive_with_exclusions(
    dir: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
    symbols: &mut BTreeSet<String>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let path = entry.path();
        if ft.is_dir() && !ft.is_symlink() {
            if !exclusions.can_prune_directory(&path) {
                scan_dir_recursive_with_exclusions(&path, exclusions, symbols);
            }
        } else if (ft.is_file() || (ft.is_symlink() && path.is_file()))
            && !exclusions.is_excluded_path(&path)
            && matches!(path.extension().and_then(|e| e.to_str()), Some("R" | "r"))
            && let Ok(content) = fs::read_to_string(&path)
        {
            extract_sysdata_names_from_source(&content, symbols);
        }
    }
}

/// Extract sysdata symbol names from a single R source file's content.
pub fn extract_sysdata_names_from_source(content: &str, symbols: &mut BTreeSet<String>) {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .is_err()
    {
        return;
    }
    let Some(tree) = parser.parse(content, None) else {
        return;
    };
    visit_for_sysdata(tree.root_node(), content, symbols);
}

fn visit_for_sysdata(node: Node, content: &str, symbols: &mut BTreeSet<String>) {
    if node.kind() == "call" {
        try_extract_use_data_internal(node, content, symbols);
        try_extract_save_sysdata(node, content, symbols);
    }
    for child in node.children(&mut node.walk()) {
        visit_for_sysdata(child, content, symbols);
    }
}

/// Match `usethis::use_data(a, b, internal = TRUE)`, `devtools::use_data(...)`,
/// or bare `use_data(a, b, internal = TRUE)`.
fn try_extract_use_data_internal(node: Node, content: &str, symbols: &mut BTreeSet<String>) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    let func_text = node_text(func_node, content);
    // `devtools::use_data` is the historic re-export of `usethis::use_data`
    // (e.g. readr's data-raw/date-symbols.R); all three spellings write the
    // same R/sysdata.rda.
    if func_text != "use_data"
        && func_text != "usethis::use_data"
        && func_text != "devtools::use_data"
    {
        return;
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return;
    };
    if !has_named_bool_arg(&args_node, content, "internal", true) {
        return;
    }
    // Collect positional (unnamed) arguments that are bare identifiers
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
            && value_node.kind() == "identifier"
        {
            let name = node_text(value_node, content);
            if !name.is_empty() {
                symbols.insert(name.to_string());
            }
        }
    }
}

/// Match `save(a, b, file = "...sysdata.rda")` or `save(list = c("a","b"), file = "...sysdata.rda")`.
fn try_extract_save_sysdata(node: Node, content: &str, symbols: &mut BTreeSet<String>) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    if node_text(func_node, content) != "save" {
        return;
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return;
    };
    if !file_arg_is_sysdata(&args_node, content) {
        return;
    }
    // Try `list = c("a", "b")` form
    if let Some(list_names) = extract_list_arg_strings(&args_node, content) {
        symbols.extend(list_names);
        return;
    }
    // Positional bare identifiers (excluding named args)
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
            && value_node.kind() == "identifier"
        {
            let name = node_text(value_node, content);
            if !name.is_empty() {
                symbols.insert(name.to_string());
            }
        }
    }
}

/// Check whether the `save(file = "...")` literal points at a package's
/// `R/sysdata.rda` (the two conventional spellings `sysdata.rda` and
/// `sysdata.RData`).
///
/// Matches on the path's final component only, exactly equal to one of those
/// spellings. A loose substring test wrongly matched any path merely *containing*
/// the token, e.g. `backup/mysysdata.rda.old` or `notsysdata.rda`; those are not
/// `R/sysdata.rda` and must not feed the sysdata symbol set. The two accepted
/// spellings preserve the existing case behavior (the stem is always lowercase
/// `sysdata`; only the `.rda`/`.RData` extension casing differs).
fn file_arg_is_sysdata(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == "file"
            && let Some(value_node) = child.child_by_field_name("value")
            && let Some(s) = extract_string_literal(value_node, content)
        {
            // Final path component, splitting on both Unix and Windows separators.
            let file_name = s.rsplit(['/', '\\']).next().unwrap_or(&s);
            return file_name == "sysdata.rda" || file_name == "sysdata.RData";
        }
    }
    false
}

/// Extract string literals from `list = c("a", "b", ...)`.
fn extract_list_arg_strings(args_node: &Node, content: &str) -> Option<Vec<String>> {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == "list"
            && let Some(value_node) = child.child_by_field_name("value")
        {
            return extract_c_string_literals(value_node, content);
        }
    }
    None
}

/// Given a node that should be `c("a", "b", ...)`, extract the string literals.
fn extract_c_string_literals(node: Node, content: &str) -> Option<Vec<String>> {
    if node.kind() != "call" {
        return None;
    }
    let func_node = node.child_by_field_name("function")?;
    if node_text(func_node, content) != "c" {
        return None;
    }
    let args_node = node.child_by_field_name("arguments")?;
    let mut result = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(value_node) = child.child_by_field_name("value")
            && let Some(s) = extract_string_literal(value_node, content)
        {
            result.push(s);
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

// === .onLoad / .onAttach binding extraction ===

/// Extract symbols bound in `.onLoad` and `.onAttach` function bodies from
/// `R/*.R` source text. Detects:
/// - `assign("x", ..., envir = ...)` at top level of the hook body
/// - `ns$x <- ...` / `topenv()$x <- ...`
pub fn extract_onload_bindings(content: &str) -> BTreeSet<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .is_err()
    {
        return BTreeSet::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return BTreeSet::new();
    };
    let mut symbols = BTreeSet::new();
    visit_for_onload(tree.root_node(), content, &mut symbols);
    symbols
}

fn visit_for_onload(node: Node, content: &str, symbols: &mut BTreeSet<String>) {
    if is_onload_definition(node, content) {
        if let Some(rhs) = node.child_by_field_name("rhs")
            && rhs.kind() == "function_definition"
            && let Some(body) = rhs.child_by_field_name("body")
        {
            extract_bindings_from_body(body, content, symbols);
        }
        return;
    }
    for child in node.children(&mut node.walk()) {
        visit_for_onload(child, content, symbols);
    }
}

/// Check if this node is a top-level `<-` assignment where the LHS is
/// `.onLoad` or `.onAttach`.
fn is_onload_definition(node: Node, content: &str) -> bool {
    if node.kind() != "binary_operator" {
        return false;
    }
    let Some(op) = node.child_by_field_name("operator") else {
        return false;
    };
    if !matches!(node_text(op, content), "<-" | "=") {
        return false;
    }
    let Some(lhs) = node.child_by_field_name("lhs") else {
        return false;
    };
    if lhs.kind() != "identifier" {
        return false;
    }
    let name = node_text(lhs, content);
    name == ".onLoad" || name == ".onAttach"
}

fn extract_bindings_from_body(body: Node, content: &str, symbols: &mut BTreeSet<String>) {
    // Functions defined *locally* within the hook body: `environment(<fn>)` for
    // one of these returns the hook's own execution frame, NOT the package
    // namespace, so such an identifier must not be treated as namespace-like.
    let local_fns = collect_local_function_names(body, content);
    // First pass: identify identifiers bound to the namespace env via
    // `<ident> <- topenv(...)` / `asNamespace(...)` / `getNamespace(...)`.
    let ns_idents = collect_namespace_bound_idents(body, content, &local_fns);
    visit_body_for_bindings(body, content, symbols, &ns_idents, &local_fns, false);
}

/// Collect the names of functions defined locally within the hook body
/// (`name <- function(...)` at any depth). `environment(name)` for such a
/// local function yields the hook's own frame, not the package namespace, so
/// these names must be excluded from the `environment(<identifier>)` idiom.
fn collect_local_function_names(body: Node, content: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    collect_local_function_names_rec(body, content, &mut names);
    names
}

fn collect_local_function_names_rec(node: Node, content: &str, names: &mut BTreeSet<String>) {
    if node.kind() == "binary_operator"
        && let Some(op) = node.child_by_field_name("operator")
        && matches!(node_text(op, content), "<-" | "=" | "<<-")
        && let Some(lhs) = node.child_by_field_name("lhs")
        && lhs.kind() == "identifier"
        && let Some(rhs) = node.child_by_field_name("rhs")
        && rhs.kind() == "function_definition"
    {
        names.insert(node_text(lhs, content).to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_local_function_names_rec(child, content, names);
    }
}

/// Scan top-level assignments in the hook body for patterns that bind an
/// identifier to the namespace environment (e.g. `ns <- topenv(environment())`).
/// Returns the set of identifier names that can be treated as namespace-like.
fn collect_namespace_bound_idents(
    body: Node,
    content: &str,
    local_fns: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut idents = BTreeSet::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "binary_operator"
            && let Some(op) = child.child_by_field_name("operator")
            && matches!(node_text(op, content), "<-" | "=")
            && let Some(lhs) = child.child_by_field_name("lhs")
            && lhs.kind() == "identifier"
            && let Some(rhs) = child.child_by_field_name("rhs")
            && is_namespace_creating_expr(rhs, content, local_fns)
        {
            idents.insert(node_text(lhs, content).to_string());
        }
    }
    idents
}

/// Check if a node is an expression that produces a namespace environment:
/// `topenv(...)`, `asNamespace(...)`, `getNamespace(...)`,
/// `parent.env(environment())`, or `environment(<package-level identifier>)`.
/// `parent.env` only qualifies when its first argument is itself a
/// namespace-shaped expression — `parent.env` of an arbitrary local
/// environment is NOT the namespace, so treating every `parent.env(...)` as
/// namespace-producing would over-collect and mute real undefined-variable
/// diagnostics. Likewise `environment(<identifier>)` only qualifies for a
/// package-level function, not one defined locally in the hook (see
/// `environment_arg_is_identifier`).
fn is_namespace_creating_expr(node: Node, content: &str, local_fns: &BTreeSet<String>) -> bool {
    if node.kind() != "call" {
        return false;
    }
    let Some(func_node) = node.child_by_field_name("function") else {
        return false;
    };
    match node_text(func_node, content) {
        "topenv" | "asNamespace" | "getNamespace" => true,
        "parent.env" => parent_env_arg_is_namespace_shaped(node, content, local_fns),
        "environment" => environment_arg_is_identifier(node, content, local_fns),
        _ => false,
    }
}

/// `environment(<identifier>)` returns the environment a closure was defined
/// in; for a top-level package function (the conventional `environment(dummy)`
/// idiom in `.onLoad`) that is the package namespace. Only the single-bare-
/// identifier form qualifies — `environment()` (the current local frame) and
/// `environment(<complex expr>)` do not. A function defined *locally* within
/// the hook body is also excluded: its closure environment is the hook's own
/// execution frame, not the namespace, so binding into it would mute real
/// undefined-name diagnostics.
fn environment_arg_is_identifier(call: Node, content: &str, local_fns: &BTreeSet<String>) -> bool {
    let Some(args_node) = call.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args_node.walk();
    let mut args = args_node
        .children(&mut cursor)
        .filter(|c| c.kind() == "argument");
    let Some(first) = args.next() else {
        return false;
    };
    // Exactly one positional argument that is a bare identifier.
    if args.next().is_some() || first.child_by_field_name("name").is_some() {
        return false;
    }
    let Some(value) = first.child_by_field_name("value") else {
        return false;
    };
    if value.kind() != "identifier" {
        return false;
    }
    // A locally-defined function's environment is the hook frame, not the
    // namespace — only an external (package-level) function qualifies.
    !local_fns.contains(node_text(value, content))
}

/// The first positional argument of a `parent.env(...)` call is namespace-shaped:
/// a bare `environment()` (whose parent in `.onLoad` is the namespace) or a
/// nested namespace-producing call.
fn parent_env_arg_is_namespace_shaped(
    call: Node,
    content: &str,
    local_fns: &BTreeSet<String>,
) -> bool {
    let Some(args_node) = call.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" && child.child_by_field_name("name").is_none() {
            let Some(value_node) = child.child_by_field_name("value") else {
                return false;
            };
            if value_node.kind() != "call" {
                return false;
            }
            let Some(f) = value_node.child_by_field_name("function") else {
                return false;
            };
            if node_text(f, content) == "environment" {
                // Only a *bare* `environment()` is namespace-shaped here. An
                // `environment(<expr>)` form falls through to
                // `is_namespace_creating_expr`, which excludes locally-defined
                // functions so we don't mute real undefined-name diagnostics.
                let Some(inner_args) = value_node.child_by_field_name("arguments") else {
                    return false;
                };
                let mut inner_cursor = inner_args.walk();
                let is_bare = !inner_args
                    .children(&mut inner_cursor)
                    .any(|n| n.kind() == "argument");
                if is_bare {
                    return true;
                }
            }
            return is_namespace_creating_expr(value_node, content, local_fns);
        }
    }
    false
}

/// Check if a node is a namespace-like expression: a namespace-producing call
/// or an identifier previously bound to the namespace.
fn is_namespace_like(
    node: Node,
    content: &str,
    ns_idents: &BTreeSet<String>,
    local_fns: &BTreeSet<String>,
) -> bool {
    match node.kind() {
        "call" => is_namespace_creating_expr(node, content, local_fns),
        "identifier" => ns_idents.contains(node_text(node, content)),
        _ => false,
    }
}

fn visit_body_for_bindings(
    node: Node,
    content: &str,
    symbols: &mut BTreeSet<String>,
    ns_idents: &BTreeSet<String>,
    local_fns: &BTreeSet<String>,
    inside_nested_function: bool,
) {
    match node.kind() {
        "call" if !inside_nested_function => {
            try_extract_assign_call(node, content, symbols, ns_idents, local_fns);
            try_extract_make_active_binding_call(node, content, symbols, ns_idents, local_fns);
            for child in node.children(&mut node.walk()) {
                visit_body_for_bindings(
                    child,
                    content,
                    symbols,
                    ns_idents,
                    local_fns,
                    inside_nested_function,
                );
            }
        }
        "binary_operator" if !inside_nested_function => {
            try_extract_dollar_assignment(node, content, symbols, ns_idents, local_fns);
            for child in node.children(&mut node.walk()) {
                visit_body_for_bindings(
                    child,
                    content,
                    symbols,
                    ns_idents,
                    local_fns,
                    inside_nested_function,
                );
            }
        }
        "function_definition" => {
            for child in node.children(&mut node.walk()) {
                visit_body_for_bindings(child, content, symbols, ns_idents, local_fns, true);
            }
        }
        _ => {
            for child in node.children(&mut node.walk()) {
                visit_body_for_bindings(
                    child,
                    content,
                    symbols,
                    ns_idents,
                    local_fns,
                    inside_nested_function,
                );
            }
        }
    }
}

/// Match `assign("x", value, envir = <ns>)` — extract "x" only when the
/// `envir` target is namespace-like (a tracked ns identifier or `topenv(...)`).
fn try_extract_assign_call(
    node: Node,
    content: &str,
    symbols: &mut BTreeSet<String>,
    ns_idents: &BTreeSet<String>,
    local_fns: &BTreeSet<String>,
) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    if node_text(func_node, content) != "assign" {
        return;
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return;
    };
    // Check envir arg is namespace-like
    if !envir_is_namespace_like(&args_node, content, ns_idents, local_fns) {
        return;
    }
    // First positional arg should be a string literal with the name
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
            && let Some(name) = extract_string_literal(value_node, content)
        {
            if !name.is_empty() {
                symbols.insert(name);
            }
            return;
        }
    }
}

/// Match `makeActiveBinding("sym", fun, env)` — extract "sym" only when the
/// `env` target (3rd positional argument or named `env =`) is namespace-like.
///
/// `makeActiveBinding(sym, fun, env)` installs an active binding named `sym`
/// in `env`. In `.onLoad` the conventional `env` is the package namespace
/// (e.g. cli's `pkgenv <- environment(dummy)`), so the binding is a
/// package-internal symbol. Only the statically-safe shape is recognized: the
/// `sym` argument (1st positional or named `sym =`) is a string literal and
/// the `env` target is namespace-like — mirroring [`try_extract_assign_call`].
fn try_extract_make_active_binding_call(
    node: Node,
    content: &str,
    symbols: &mut BTreeSet<String>,
    ns_idents: &BTreeSet<String>,
    local_fns: &BTreeSet<String>,
) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    if node_text(func_node, content) != "makeActiveBinding" {
        return;
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return;
    };

    // Resolve the `sym` (name) and `env` (target) arguments, honoring both
    // positional order (sym, fun, env) and explicit names.
    let mut sym_value = None;
    let mut env_value = None;
    let mut positional_index = 0;
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if let Some(name_node) = child.child_by_field_name("name") {
            match node_text(name_node, content) {
                "sym" => sym_value = child.child_by_field_name("value"),
                "env" => env_value = child.child_by_field_name("value"),
                _ => {}
            }
        } else {
            match positional_index {
                0 => sym_value = child.child_by_field_name("value"),
                2 => env_value = child.child_by_field_name("value"),
                _ => {}
            }
            positional_index += 1;
        }
    }

    // The target environment must be namespace-like.
    let Some(env_node) = env_value else {
        return;
    };
    if !is_namespace_like(env_node, content, ns_idents, local_fns) {
        return;
    }

    let Some(sym_node) = sym_value else {
        return;
    };
    if let Some(name) = extract_string_literal(sym_node, content)
        && !name.is_empty()
    {
        symbols.insert(name);
    }
}

/// Check if the `envir` named argument is a namespace-like expression.
fn envir_is_namespace_like(
    args_node: &Node,
    content: &str,
    ns_idents: &BTreeSet<String>,
    local_fns: &BTreeSet<String>,
) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == "envir"
            && let Some(value_node) = child.child_by_field_name("value")
        {
            return is_namespace_like(value_node, content, ns_idents, local_fns);
        }
    }
    false
}

/// Match `<ns>$x <- ...` or `topenv()$x <- ...` patterns — only when the
/// receiver is namespace-like.
fn try_extract_dollar_assignment(
    node: Node,
    content: &str,
    symbols: &mut BTreeSet<String>,
    ns_idents: &BTreeSet<String>,
    local_fns: &BTreeSet<String>,
) {
    // Must be an assignment operator
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    if !matches!(node_text(op, content), "<-" | "<<-" | "=") {
        return;
    }
    let Some(lhs) = node.child_by_field_name("lhs") else {
        return;
    };
    if lhs.kind() != "extract_operator" {
        return;
    }
    let mut cursor = lhs.walk();
    let children: Vec<_> = lhs.children(&mut cursor).collect();
    if children.len() < 3 {
        return;
    }
    if node_text(children[1], content) != "$" {
        return;
    }
    // Check that the receiver (children[0]) is namespace-like
    if !is_namespace_like(children[0], content, ns_idents, local_fns) {
        return;
    }
    let field = &children[2];
    let name = if field.kind() == "string" {
        extract_string_literal(*field, content)
    } else if field.kind() == "identifier" {
        Some(node_text(*field, content).to_string())
    } else {
        None
    };
    if let Some(n) = name
        && !n.is_empty()
    {
        symbols.insert(n);
    }
}

// === Helpers ===

fn has_named_bool_arg(args_node: &Node, content: &str, param: &str, expected: bool) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == param
            && let Some(value_node) = child.child_by_field_name("value")
        {
            let val = node_text(value_node, content);
            return if expected {
                val == "TRUE" || val == "T"
            } else {
                val == "FALSE" || val == "F"
            };
        }
    }
    false
}

fn extract_string_literal(node: Node, content: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let text = node_text(node, content);
    if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\''))
    {
        Some(text[1..text.len() - 1].to_string())
    } else {
        None
    }
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

// === R subprocess fallback ===

use std::sync::Mutex;

type SysdataCache = Mutex<
    Option<(
        crate::r_subprocess::RRuntimeIdentity,
        super::ContentDigest,
        BTreeSet<String>,
    )>,
>;

/// Cached result keyed by the exact R runtime and sysdata content identities.
static SYSDATA_R_CACHE: std::sync::LazyLock<SysdataCache> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Frozen byte identity of `R/sysdata.rda` consumed by detached fallback work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SysdataFileObservation {
    Valid {
        digest: super::ContentDigest,
        bytes: std::sync::Arc<[u8]>,
    },
    Missing,
    Invalid,
}

impl SysdataFileObservation {
    pub(crate) fn capture(workspace_root: &Path) -> Self {
        let path = workspace_root.join("R").join("sysdata.rda");
        match fs::read(&path) {
            Ok(bytes) if path.is_file() => Self::Valid {
                digest: super::ContentDigest::of_bytes(&bytes),
                bytes: bytes.into(),
            },
            Ok(_) => Self::Invalid,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            Err(_) => Self::Invalid,
        }
    }

    pub(crate) fn is_current(&self, workspace_root: &Path) -> bool {
        let current = Self::capture(workspace_root);
        match (self, current) {
            (
                Self::Valid {
                    digest: expected, ..
                },
                Self::Valid {
                    digest: current, ..
                },
            ) => *expected == current,
            (Self::Missing, Self::Missing) | (Self::Invalid, Self::Invalid) => true,
            _ => false,
        }
    }
}

/// R fallback: load `R/sysdata.rda` via an R subprocess and return `ls()` of
/// the loaded environment. Called only when AST scanning found nothing AND
/// `R/sysdata.rda` exists. Caches by file digest; fail-soft (returns empty on
/// any failure).
///
/// # Safety invariants (per `r_subprocess` module doc)
/// - No user-controlled input is interpolated into the R code.
/// - Runs through `execute_r_code_with_timeout` (10s), so the R-subprocess
///   concurrency-semaphore permit is acquired outside the timeout and the call
///   is bounded against hung processes.
pub async fn load_sysdata_via_r(
    r_subprocess: &crate::r_subprocess::RSubprocess,
    workspace_root: &Path,
) -> BTreeSet<String> {
    let observation = SysdataFileObservation::capture(workspace_root);
    let SysdataFileObservation::Valid { digest, bytes } = observation else {
        return BTreeSet::new();
    };
    load_frozen_sysdata_via_r(r_subprocess, digest, &bytes)
        .await
        .unwrap_or_default()
}

/// Load an immutable byte snapshot through R.
///
/// The temporary file ensures the subprocess cannot observe a rewrite of the
/// workspace path between capture and `load()`. Callers must still revalidate
/// their original [`SysdataFileObservation`] immediately before committing the
/// derived package projection.
pub(crate) async fn load_frozen_sysdata_via_r(
    r_subprocess: &crate::r_subprocess::RSubprocess,
    digest: super::ContentDigest,
    bytes: &[u8],
) -> Result<BTreeSet<String>, String> {
    let runtime_identity = r_subprocess.runtime_identity();
    // Check cache
    if let Ok(guard) = SYSDATA_R_CACHE.lock()
        && let Some((cached_runtime, cached_digest, cached_symbols)) = guard.as_ref()
        && cached_runtime == &runtime_identity
        && *cached_digest == digest
    {
        return Ok(cached_symbols.clone());
    }

    use std::io::Write;
    let mut frozen = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    frozen
        .as_file_mut()
        .write_all(bytes)
        .and_then(|_| frozen.as_file_mut().flush())
        .map_err(|error| error.to_string())?;

    // Build R code. The path is workspace-derived, but filesystem paths can
    // legally contain `"`, `\`, and control characters, so escape it into a
    // safe R string literal rather than interpolating verbatim.
    let sysdata_path_str = frozen.path().to_string_lossy().replace('\\', "/");
    let escaped_path: String = sysdata_path_str
        .chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            other => vec![other],
        })
        .collect();
    let r_code = format!(
        r#"e <- new.env(parent = emptyenv()); load("{}", e); cat(ls(e), sep = "\n")"#,
        escaped_path
    );

    // Use `execute_r_code_with_timeout` (not an outer `tokio::time::timeout`
    // around `execute_r_code`) so the global R-subprocess semaphore permit is
    // acquired OUTSIDE the timeout: queue-wait under R-subprocess contention
    // must not count against this 10s budget, otherwise a busy session could
    // time out here and fail-soft to an empty set, reintroducing the very
    // package-internal-sysdata false positives this fallback exists to prevent.
    let result = match r_subprocess
        .execute_r_code_with_timeout(&r_code, std::time::Duration::from_secs(10))
        .await
    {
        Ok(stdout) => stdout,
        Err(error) => return Err(error.to_string()),
    };

    let symbols: BTreeSet<String> = result
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Update cache
    if let Ok(mut guard) = SYSDATA_R_CACHE.lock() {
        *guard = Some((runtime_identity, digest, symbols.clone()));
    }

    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- use_data with internal = TRUE ---

    #[test]
    fn use_data_internal_true_extracts_symbols() {
        let code = r#"usethis::use_data(x, y, internal = TRUE)"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("x"), "got: {:?}", syms);
        assert!(syms.contains("y"), "got: {:?}", syms);
    }

    #[test]
    fn devtools_use_data_internal_true_extracts_symbols() {
        // readr's data-raw/date-symbols.R idiom: the historic devtools::use_data
        // re-export must be recognized like usethis::use_data.
        let code = r#"devtools::use_data(date_symbols, internal = TRUE, overwrite = TRUE)"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("date_symbols"), "got: {:?}", syms);
    }

    #[test]
    fn use_data_without_internal_does_not_feed_sysdata() {
        let code = r#"usethis::use_data(d)"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.is_empty(), "got: {:?}", syms);
    }

    #[test]
    fn use_data_internal_false_does_not_feed_sysdata() {
        let code = r#"use_data(d, internal = FALSE)"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.is_empty(), "got: {:?}", syms);
    }

    #[test]
    fn use_data_bare_name_extracts() {
        let code = r#"use_data(alpha, beta, internal = TRUE)"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("alpha"), "got: {:?}", syms);
        assert!(syms.contains("beta"), "got: {:?}", syms);
    }

    // --- save() with sysdata.rda ---

    #[test]
    fn save_with_sysdata_file_extracts_positional_symbols() {
        let code = r#"save(z, file = "R/sysdata.rda")"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("z"), "got: {:?}", syms);
    }

    #[test]
    fn save_with_list_arg_extracts_strings() {
        let code = r#"save(list = c("a", "b"), file = "R/sysdata.rda")"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("a"), "got: {:?}", syms);
        assert!(syms.contains("b"), "got: {:?}", syms);
    }

    #[test]
    fn save_without_sysdata_does_not_extract() {
        let code = r#"save(z, file = "data/z.rda")"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.is_empty(), "got: {:?}", syms);
    }

    // FIX 4: the `file=` match is on the final path component, exactly equal to
    // `sysdata.rda` / `sysdata.RData` — a path that merely *contains* the token
    // must not feed the sysdata symbol set.
    #[test]
    fn save_to_path_merely_containing_sysdata_token_does_not_extract() {
        for path in [
            "backup/mysysdata.rda.old", // token in the middle, wrong final component
            "notsysdata.rda",           // longer stem
            "sysdata.rda.bak",          // trailing suffix
            "R/sysdata.RDATA",          // wrong extension casing
            "presysdata.RData",
        ] {
            let code = format!(r#"save(z, file = "{path}")"#);
            let mut syms = BTreeSet::new();
            extract_sysdata_names_from_source(&code, &mut syms);
            assert!(
                syms.is_empty(),
                "path {path:?} must not match, got: {syms:?}"
            );
        }
    }

    #[test]
    fn save_with_windows_separator_sysdata_extracts() {
        // The accepted spellings still match on a Windows-style path.
        let code = r#"save(z, file = "R\\sysdata.rda")"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("z"), "got: {:?}", syms);
    }

    #[test]
    fn save_with_rdata_casing_extracts() {
        // Regression guard: the `sysdata.RData` spelling is still accepted.
        let code = r#"save(z, file = "R/sysdata.RData")"#;
        let mut syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut syms);
        assert!(syms.contains("z"), "got: {:?}", syms);
    }

    // --- .onLoad / .onAttach bindings ---

    #[test]
    fn onload_assign_with_envir_extracts() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- topenv(environment())
  assign("x", 42, envir = ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("x"), "got: {:?}", syms);
    }

    #[test]
    fn onload_ns_dollar_assignment_extracts() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- topenv(environment())
  ns$my_var <- compute_something()
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("my_var"), "got: {:?}", syms);
    }

    #[test]
    fn onattach_dollar_assignment_extracts() {
        let code = r#"
.onAttach <- function(libname, pkgname) {
  topenv()$greeting <- "hello"
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("greeting"), "got: {:?}", syms);
    }

    #[test]
    fn onload_make_active_binding_environment_idiom_extracts() {
        // cli's idiom: `pkgenv <- environment(dummy)` then makeActiveBinding
        // into pkgenv. The literal name becomes a package-internal symbol.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  pkgenv <- environment(dummy)
  makeActiveBinding(
    "symbol",
    function() compute(),
    pkgenv
  )
  makeActiveBinding("pb_bar", cli__pb_bar, pkgenv)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("symbol"), "got: {:?}", syms);
        assert!(syms.contains("pb_bar"), "got: {:?}", syms);
    }

    #[test]
    fn onload_make_active_binding_named_args_extracts() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- topenv(environment())
  makeActiveBinding(sym = "active_sym", fun = function() 1, env = ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("active_sym"), "got: {:?}", syms);
    }

    #[test]
    fn onload_make_active_binding_non_ns_env_not_collected() {
        // Binding into a fresh local environment is not a package symbol.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  e <- new.env(parent = emptyenv())
  makeActiveBinding("tmp", function() 1, e)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(!syms.contains("tmp"), "got: {:?}", syms);
    }

    #[test]
    fn onload_environment_of_local_function_not_namespace() {
        // `local_fun` is defined inside .onLoad, so `environment(local_fun)` is
        // the hook's own frame, NOT the package namespace. A binding into it
        // must not be collected as a package symbol (would mute real
        // undefined-name diagnostics elsewhere).
        let code = r#"
.onLoad <- function(libname, pkgname) {
  local_fun <- function() NULL
  env <- environment(local_fun)
  makeActiveBinding("x", function() 1, env)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(!syms.contains("x"), "got: {:?}", syms);
    }

    #[test]
    fn onload_environment_of_local_function_direct_form_not_namespace() {
        // Same as above but the local-function environment is passed inline as
        // the makeActiveBinding target.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  local_fun <- function() NULL
  makeActiveBinding("x", function() 1, environment(local_fun))
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(!syms.contains("x"), "got: {:?}", syms);
    }

    #[test]
    fn onload_make_active_binding_dynamic_name_ignored() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- topenv(environment())
  makeActiveBinding(name_var, function() 1, ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.is_empty(), "got: {:?}", syms);
    }

    #[test]
    fn make_active_binding_outside_onload_ignored() {
        // makeActiveBinding is only scanned inside .onLoad/.onAttach hooks.
        let code = r#"
my_func <- function() {
  ns <- topenv(environment())
  makeActiveBinding("x", function() 1, ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(!syms.contains("x"), "got: {:?}", syms);
    }

    #[test]
    fn local_assign_in_ordinary_function_stays_local() {
        let code = r#"
my_func <- function() {
  assign("x", 42, envir = ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.is_empty(), "got: {:?}", syms);
    }

    #[test]
    fn nested_function_inside_onload_stays_local() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- topenv(environment())
  helper <- function() {
    assign("local_only", 1, envir = e)
  }
  assign("visible", 2, envir = ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("visible"), "got: {:?}", syms);
        assert!(!syms.contains("local_only"), "got: {:?}", syms);
    }

    #[test]
    fn absent_everything_is_noop() {
        let code = r#"
foo <- function(x) x + 1
bar <- 42
"#;
        let mut sysdata_syms = BTreeSet::new();
        extract_sysdata_names_from_source(code, &mut sysdata_syms);
        assert!(sysdata_syms.is_empty());
        let onload_syms = extract_onload_bindings(code);
        assert!(onload_syms.is_empty());
    }

    // --- .onLoad namespace-awareness (Group E finding 10) ---

    #[test]
    fn onload_assign_to_non_ns_envir_not_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  e <- new.env(parent = emptyenv())
  assign("tmp", 1, envir = e)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            !syms.contains("tmp"),
            "local assign should not be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_dollar_assign_to_non_ns_not_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  cache <- new.env(parent = emptyenv())
  cache$foo <- 1
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            !syms.contains("foo"),
            "local $ assign should not be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_assign_to_topenv_call_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  assign("bar", 99, envir = topenv(environment()))
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            syms.contains("bar"),
            "topenv() envir should be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_dollar_assign_to_topenv_call_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  topenv(environment())$baz <- "hello"
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            syms.contains("baz"),
            "topenv()$ should be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_ns_via_as_namespace_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  nsenv <- asNamespace(pkgname)
  assign("exported_fn", my_fn, envir = nsenv)
  nsenv$another <- 42
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            syms.contains("exported_fn"),
            "asNamespace assign: {:?}",
            syms
        );
        assert!(syms.contains("another"), "asNamespace $ assign: {:?}", syms);
    }

    #[test]
    fn onload_ns_via_get_namespace_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- getNamespace(pkgname)
  assign("dynamic", value, envir = ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(syms.contains("dynamic"), "getNamespace assign: {:?}", syms);
    }

    #[test]
    fn onload_parent_env_environment_collected() {
        // parent.env(environment()) in .onLoad IS the namespace.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- parent.env(environment())
  assign("pe_sym", value, envir = ns)
  ns$pe_dollar <- 1
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            syms.contains("pe_sym"),
            "parent.env(environment()) assign: {:?}",
            syms
        );
        assert!(
            syms.contains("pe_dollar"),
            "parent.env(environment()) $ assign: {:?}",
            syms
        );
    }

    #[test]
    fn onload_parent_env_local_not_collected() {
        // parent.env(<local env>) is NOT the namespace — must not be collected.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  e <- new.env(parent = emptyenv())
  assign("local_pe", 1, envir = parent.env(e))
  parent.env(e)$local_pe_dollar <- 2
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            !syms.contains("local_pe"),
            "parent.env(local) assign must not be collected: {:?}",
            syms
        );
        assert!(
            !syms.contains("local_pe_dollar"),
            "parent.env(local) $ assign must not be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_parent_env_environment_of_local_function_not_collected() {
        // `parent.env(environment(local_fun))`: `environment(local_fun)` is the
        // hook's own frame (local_fun is defined inside .onLoad), so its parent
        // is NOT the package namespace. The local-function exclusion must hold
        // through the `parent.env(...)` wrapper too.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  local_fun <- function() NULL
  ns <- parent.env(environment(local_fun))
  assign("pe_local", 1, envir = ns)
  ns$pe_local_dollar <- 2
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            !syms.contains("pe_local"),
            "parent.env(environment(local_fun)) assign must not be collected: {:?}",
            syms
        );
        assert!(
            !syms.contains("pe_local_dollar"),
            "parent.env(environment(local_fun)) $ assign must not be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_parent_env_environment_of_external_function_collected() {
        // `parent.env(environment(pkg_fun))` where `pkg_fun` is a package-level
        // (external) function: `environment(pkg_fun)` is the namespace, so its
        // parent qualifies as namespace-shaped and the binding is collected.
        let code = r#"
.onLoad <- function(libname, pkgname) {
  ns <- parent.env(environment(pkg_fun))
  assign("pe_ext", 1, envir = ns)
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            syms.contains("pe_ext"),
            "parent.env(environment(external_fun)) assign should be collected: {:?}",
            syms
        );
    }

    #[test]
    fn onload_inline_as_namespace_envir_collected() {
        let code = r#"
.onLoad <- function(libname, pkgname) {
  assign("inlined", val, envir = asNamespace(pkgname))
  getNamespace(pkgname)$dollar_inline <- TRUE
}
"#;
        let syms = extract_onload_bindings(code);
        assert!(
            syms.contains("inlined"),
            "inline asNamespace envir: {:?}",
            syms
        );
        assert!(
            syms.contains("dollar_inline"),
            "inline getNamespace $ assign: {:?}",
            syms
        );
    }

    // --- Filesystem scan ---

    #[test]
    fn scan_sysdata_generating_scripts_from_data_raw() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_raw = tmp.path().join("data-raw");
        std::fs::create_dir(&data_raw).unwrap();
        std::fs::write(
            data_raw.join("prepare.R"),
            "usethis::use_data(x, y, internal = TRUE)\n",
        )
        .unwrap();
        let syms = scan_sysdata_generating_scripts(tmp.path());
        assert!(syms.contains("x"), "got: {:?}", syms);
        assert!(syms.contains("y"), "got: {:?}", syms);
    }

    #[test]
    fn scan_sysdata_generating_scripts_with_exclusions_skips_data_raw_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_raw = tmp.path().join("data-raw");
        std::fs::create_dir(&data_raw).unwrap();
        std::fs::write(
            data_raw.join("included.R"),
            "usethis::use_data(included_sysdata, internal = TRUE)\n",
        )
        .unwrap();
        std::fs::write(
            data_raw.join("excluded.R"),
            "usethis::use_data(excluded_sysdata, internal = TRUE)\n",
        )
        .unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &serde_json::json!({ "workspace": { "exclude": ["data-raw/excluded.R"] } }),
            vec![tmp.path().to_path_buf()],
        );

        let syms = scan_sysdata_generating_scripts_with_exclusions(tmp.path(), &exclusions);

        assert!(syms.contains("included_sysdata"), "got: {:?}", syms);
        assert!(
            !syms.contains("excluded_sysdata"),
            "excluded data-raw/*.R must not seed sysdata names: {:?}",
            syms
        );
    }

    #[test]
    fn scan_sysdata_no_data_raw_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let syms = scan_sysdata_generating_scripts(tmp.path());
        assert!(syms.is_empty());
    }

    /// R fallback test: loads a committed sysdata.rda fixture via R subprocess.
    /// Skip-gated: only runs when R is available.
    #[tokio::test]
    async fn r_fallback_loads_sysdata_rda_fixture() {
        let Some(r) = crate::r_subprocess::RSubprocess::new(None) else {
            eprintln!("skipping r_fallback_loads_sysdata_rda_fixture: R not available");
            return;
        };
        let fixture_root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sysdata_pkg");
        let names = super::load_sysdata_via_r(&r, &fixture_root).await;
        assert!(
            names.contains("sysdata_var1"),
            "expected sysdata_var1, got: {:?}",
            names
        );
        assert!(
            names.contains("sysdata_var2"),
            "expected sysdata_var2, got: {:?}",
            names
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_dir_recursive_skips_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_raw = tmp.path().join("data-raw");
        std::fs::create_dir(&data_raw).unwrap();
        // A regular R file that should be found.
        std::fs::write(
            data_raw.join("gen.R"),
            "usethis::use_data(found, internal = TRUE)\n",
        )
        .unwrap();
        // Create a symlink loop: data-raw/loop -> .. (ancestor)
        symlink(tmp.path(), data_raw.join("loop")).unwrap();

        // Must terminate despite the symlink loop.
        let syms = scan_sysdata_generating_scripts(tmp.path());
        assert!(syms.contains("found"), "expected 'found' in {:?}", syms);
    }

    #[cfg(unix)]
    #[test]
    fn scan_dir_recursive_follows_symlinked_files() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_raw = tmp.path().join("data-raw");
        std::fs::create_dir(&data_raw).unwrap();
        // Real file outside data-raw
        let real_file = tmp.path().join("real.R");
        std::fs::write(&real_file, "usethis::use_data(linked, internal = TRUE)\n").unwrap();
        // Symlink to the real file inside data-raw
        symlink(&real_file, data_raw.join("link.R")).unwrap();

        let syms = scan_sysdata_generating_scripts(tmp.path());
        assert!(
            syms.contains("linked"),
            "symlinked file should be scanned: {:?}",
            syms
        );
    }
}
