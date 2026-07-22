//
// box_use/exports.rs
//
// Parse a box module's exported interface.
//

//! Box module export parsing.
//!
//! A box module declares what it exports through one of two explicit
//! mechanisms, or falls back to a legacy default:
//!
//! 1. **`box::export(a, b, c)`** — unquoted static names. The union over all
//!    **top-level** `box::export()` calls in the file is the export set;
//!    `box::export()` calls nested inside a function body do **not** count.
//!    `box::export()` with no arguments is an explicit *empty* export set (a
//!    fully private module).
//! 2. **`#' @export`** roxygen tags on top-level definitions and on top-level
//!    `box::use()` imports (the latter *re-exports* the imported names).
//!
//! These two mechanisms are **mutually exclusive**, and `box::export()`
//! **overrides** `#' @export`: box itself consults `@export` tags only when the
//! module contains no `box::export()` call. So when any top-level
//! `box::export()` call is present, the export set is exactly the union of
//! those calls' names and every `#' @export` tag is ignored (issue #662, draft
//! problem #3). Only when there is no `box::export()` call at all do `#' @export`
//! tags define the interface.
//!
//! If **either** explicit mechanism appears, the export set is authoritative
//! ([`ExportMode::Explicit`]). If **neither** appears, the module exports every
//! default-visible top-level name — i.e. every top-level binding whose name
//! does not begin with a dot. That marker-less default is deliberately **not**
//! computed here from a parallel assignment parser: it is derived at resolution
//! time from Raven's live, function-scope- and `rm()`-aware top-level artifacts
//! (see [`super::resolve::resolve_module_export_set`] and
//! [`crate::cross_file::scope::live_top_level_exports`]).
//!
//! # Re-exports
//!
//! A `#' @export` on a `box::use()` re-exports what the import brought in.
//! Namespace aliases are known immediately; named, renamed, and wildcard
//! attachments are recorded symbolically in [`BoxExports::reexports`] and
//! resolved against the imported source's export boundary. This prevents a
//! stale/private source member from being re-exported merely because its name
//! appeared in the declaration, while preserving wildcard re-exports exactly.
//!
//! Privacy is preserved: a non-exported top-level name never crosses the
//! boundary, and a transitively-imported name is exported only when it is
//! explicitly re-exported.

use std::collections::BTreeSet;

use tree_sitter::{Node, Tree};

use super::detect::{is_box_use_call, parse_use_call_node};
use super::{BoxExports, BoxImport};
use serde::{Deserialize, Serialize};

/// How a module's export set was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportMode {
    /// At least one `box::export()` call or `#' @export` tag was present; the
    /// set is authoritative (absence may be diagnosed).
    Explicit,
    /// No explicit export marker; the set is every default-visible top-level
    /// name. Not authoritative for absence (dynamic bindings may be missed).
    LegacyDefault,
}

/// Parse a module's *explicit* export set.
///
/// Returns `Some` only when the file uses an explicit export mechanism
/// (`box::export()` and/or `#' @export`). Returns `None` for a marker-less
/// file; the marker-less legacy default is derived at resolution time from live
/// top-level artifacts, not here (see [`super::resolve::resolve_module_export_set`]).
///
/// This split keeps [`CrossFileMetadata`](crate::cross_file::CrossFileMetadata)
/// from labelling every ordinary R script as a module: only files with box
/// export markers carry a stored export set.
///
/// Precedence (draft problem #3): a top-level `box::export()` call **overrides**
/// `#' @export`. If any top-level `box::export()` call is present, the result is
/// exactly the union of those calls and all `#' @export` tags are ignored;
/// otherwise `#' @export` tags define the set.
pub fn parse_box_exports(tree: &Tree, content: &str) -> Option<BoxExports> {
    let root = tree.root_node();

    // (1) Top-level box::export(...) calls. Calls nested inside function bodies
    // are ignored — only module-level export declarations count.
    let mut box_export_members: BTreeSet<String> = BTreeSet::new();
    let mut saw_box_export = false;
    collect_top_level_box_export_calls(root, content, &mut box_export_members, &mut saw_box_export);

    // box::export() overrides @export entirely: when present, the export set is
    // exactly its union and #' @export tags are not consulted.
    if saw_box_export {
        return Some(BoxExports {
            members: box_export_members,
            mode: ExportMode::Explicit,
            reexports: Vec::new(),
        });
    }

    // (2) #' @export tags on top-level definitions and box::use imports.
    let lines: Vec<&str> = content.lines().collect();
    let mut members: BTreeSet<String> = BTreeSet::new();
    let mut reexports: Vec<BoxImport> = Vec::new();
    let mut saw_export_tag = false;
    for child in root.children(&mut root.walk()) {
        let start_line = child.start_position().row;
        if !preceded_by_export_tag(&lines, start_line) {
            continue;
        }
        if is_box_use_call(child, content) {
            saw_export_tag = true;
            // Re-export: the local names this import binds. Re-exports are
            // module-level, so function_scoped is false here.
            for imp in parse_use_call_node(child, content, false) {
                // A namespace alias is introduced independently of the source's
                // member set, so it is statically known. Attached names must be
                // validated against the resolved source export boundary: record
                // every attachment re-export symbolically, not only wildcards.
                if let Some(alias) = imp.effective_alias() {
                    members.insert(alias);
                }
                if !imp.attach.is_empty() {
                    reexports.push(imp);
                }
            }
        } else if let Some(name) = top_level_defined_name(child, content) {
            saw_export_tag = true;
            members.insert(name);
        }
    }

    if saw_export_tag {
        Some(BoxExports {
            members,
            mode: ExportMode::Explicit,
            reexports,
        })
    } else {
        None
    }
}

/// Collect names from every **top-level** `box::export(...)` call (direct
/// children of the program root). Calls nested inside function bodies are
/// deliberately skipped — only module-level export declarations are authoritative.
fn collect_top_level_box_export_calls(
    root: Node,
    content: &str,
    members: &mut BTreeSet<String>,
    saw: &mut bool,
) {
    for node in root.children(&mut root.walk()) {
        if node.kind() == "call"
            && node
                .child_by_field_name("function")
                .is_some_and(|f| is_box_export_function(f, content))
        {
            *saw = true;
            if let Some(args) = node.child_by_field_name("arguments") {
                for child in args.children(&mut args.walk()) {
                    if child.kind() != "argument" {
                        continue;
                    }
                    // Only positional, statically-named arguments.
                    if child.child_by_field_name("name").is_some() {
                        continue;
                    }
                    if let Some(value) = child.child_by_field_name("value")
                        && let Some(name) = static_name(value, content)
                    {
                        members.insert(name);
                    }
                }
            }
        }
    }
}

/// True when `node` is a `call` whose function is `box::export`.
pub(crate) fn is_box_export_call(node: Node, content: &str) -> bool {
    node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| is_box_export_function(function, content))
}

/// True for a call-function node spelling `box::export`.
fn is_box_export_function(node: Node, content: &str) -> bool {
    if node.kind() != "namespace_operator" {
        return false;
    }
    let side = |field: &str| {
        node.child_by_field_name(field)
            .map(|n| &content[n.byte_range()])
    };
    side("lhs") == Some("box") && side("rhs") == Some("export")
}

/// The static name expressed by an identifier or string-literal node.
fn static_name(node: Node, content: &str) -> Option<String> {
    match node.kind() {
        "identifier" => {
            let raw = &content[node.byte_range()];
            Some(
                raw.strip_prefix('`')
                    .and_then(|inner| inner.strip_suffix('`'))
                    .unwrap_or(raw)
                    .to_string(),
            )
        }
        "string" => {
            let raw = &content[node.byte_range()];
            let bytes = raw.as_bytes();
            if bytes.len() >= 2 {
                let first = bytes[0];
                let last = bytes[bytes.len() - 1];
                if (first == b'"' || first == b'\'' || first == b'`') && first == last {
                    return Some(raw[1..raw.len() - 1].to_string());
                }
            }
            Some(raw.to_string())
        }
        _ => None,
    }
}

/// The name a top-level statement binds, if any.
///
/// Handles `name <- ...`, `name = ...`, `name <<- ...`, and the right-assign
/// forms `... -> name`, `... ->> name`. The name side must be a bare identifier
/// or a string literal.
fn top_level_defined_name(node: Node, content: &str) -> Option<String> {
    if node.kind() != "binary_operator" {
        return None;
    }
    let op = node.child_by_field_name("operator")?;
    let op_text = &content[op.byte_range()];
    let name_node = match op_text {
        "<-" | "=" | "<<-" => node.child_by_field_name("lhs")?,
        "->" | "->>" => node.child_by_field_name("rhs")?,
        _ => return None,
    };
    static_name(name_node, content)
}

/// Whether the comment lines immediately above (0-based) `def_line` contain a
/// `#' @export` roxygen tag.
///
/// Scans upward through the source region since the previous expression. Blank
/// and comment-only lines are permitted, matching `{box}`'s `add_comments()` /
/// `has_export_tag()` parser; a preceding non-comment expression ends the region.
fn preceded_by_export_tag(lines: &[&str], def_line: usize) -> bool {
    let mut i = def_line;
    while i > 0 {
        i -= 1;
        let trimmed = lines.get(i).map(|l| l.trim_start()).unwrap_or("");
        if trimmed.starts_with("#'") {
            if is_export_tag_line(trimmed) {
                return true;
            }
            continue;
        }
        if trimmed.starts_with('#') || trimmed.is_empty() {
            // Plain comments and whitespace-only lines remain in the source
            // region attached to the next expression.
            continue;
        }
        // A preceding non-comment expression ends the region.
        break;
    }
    false
}

/// Whether a `#'` line's content is an `@export` tag (with no trailing name, as
/// box's `@export` takes no argument).
fn is_export_tag_line(roxygen_line: &str) -> bool {
    // Strip the leading `#'` and any following whitespace.
    let after = roxygen_line
        .trim_start()
        .strip_prefix("#'")
        .unwrap_or("")
        .trim_start();
    // `@export` optionally followed only by whitespace / end.
    match after.strip_prefix("@export") {
        Some(rest) => rest.trim().is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn explicit(code: &str) -> Option<BoxExports> {
        parse_box_exports(&parse(code), code)
    }

    #[test]
    fn box_export_call_union() {
        let code = "box::export(a, b)\nbox::export(c)\n";
        let ex = explicit(code).unwrap();
        assert_eq!(ex.mode, ExportMode::Explicit);
        assert_eq!(
            ex.members,
            ["a", "b", "c"].into_iter().map(String::from).collect()
        );
        assert!(ex.reexports.is_empty());
    }

    #[test]
    fn backticked_exports_use_bare_member_identity() {
        let code = "box::export(`%>%`)\n#' @export\n`pipe op` <- function(x) x\n";
        let ex = explicit(code).unwrap();
        assert_eq!(
            ex.members,
            ["%>%"].into_iter().map(String::from).collect(),
            "box::export overrides the roxygen export and strips syntax backticks"
        );

        let tagged = explicit("#' @export\n`pipe op` <- function(x) x\n").unwrap();
        assert_eq!(
            tagged.members,
            ["pipe op"].into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn empty_box_export_is_explicit_zero() {
        let ex = explicit("box::export()\nfoo <- function() 1\n").unwrap();
        assert_eq!(ex.mode, ExportMode::Explicit);
        assert!(ex.members.is_empty());
    }

    #[test]
    fn box_export_overrides_export_tags() {
        // Draft problem #3: a box::export() call wins; the #' @export tag on
        // `bar` is ignored and does NOT union in.
        let code = "#' @export\nbar <- function() 2\nbox::export(a)\n";
        let ex = explicit(code).unwrap();
        assert_eq!(ex.mode, ExportMode::Explicit);
        assert_eq!(ex.members, ["a"].into_iter().map(String::from).collect());
        assert!(!ex.members.contains("bar"));
    }

    #[test]
    fn box_export_inside_function_is_ignored() {
        // A box::export() nested in a function body does not count; the
        // top-level #' @export tag governs instead.
        let code = "#' @export\nfoo <- function() {\n  box::export(hidden)\n}\n";
        let ex = explicit(code).unwrap();
        assert_eq!(ex.members, ["foo"].into_iter().map(String::from).collect());
        assert!(!ex.members.contains("hidden"));
    }

    #[test]
    fn export_tag_on_definition() {
        let code = "#' @export\nfoo <- function() 1\n\nbar <- function() 2\n";
        let ex = explicit(code).unwrap();
        assert_eq!(ex.mode, ExportMode::Explicit);
        assert_eq!(ex.members, ["foo"].into_iter().map(String::from).collect());
    }

    #[test]
    fn export_tag_with_intervening_doc_lines() {
        let code = "#' Title.\n#'\n#' @export\nfoo <- function() 1\n";
        let ex = explicit(code).unwrap();
        assert_eq!(ex.members, ["foo"].into_iter().map(String::from).collect());
    }

    #[test]
    fn export_tag_reexports_box_use() {
        let code = "#' @export\nbox::use(dr = dplyr, ./mod[helper])\n";
        let ex = explicit(code).unwrap();
        assert!(ex.members.contains("dr"));
        assert!(!ex.members.contains("helper"));
        assert_eq!(ex.reexports.len(), 1);
    }

    #[test]
    fn wildcard_reexport_is_recorded_symbolically() {
        // Draft problem #4: `#' @export box::use(./mod[...])` cannot enumerate
        // members statically, so it is recorded as a symbolic re-export rather
        // than silently dropped.
        let code = "#' @export\nbox::use(./mod[...])\n";
        let ex = explicit(code).unwrap();
        assert_eq!(ex.reexports.len(), 1);
        assert!(matches!(
            ex.reexports[0].spec,
            crate::box_use::BoxSpec::LocalModule { .. }
        ));
        assert!(
            ex.reexports[0]
                .attach
                .iter()
                .any(|a| matches!(a, crate::box_use::BoxAttach::Wildcard))
        );
    }

    #[test]
    fn no_markers_yields_none() {
        // Marker-less file: no stored export set; the legacy default is derived
        // from live artifacts at resolution time, not here.
        let code = "foo <- function() 1\n.private <- 2\nbar = 3\n";
        assert!(explicit(code).is_none());
    }

    #[test]
    fn right_assign_export_tag_defines_name() {
        // top_level_defined_name handles right-assign under an @export tag.
        let code = "#' @export\n1 -> foo\n";
        let ex = explicit(code).unwrap();
        assert!(ex.members.contains("foo"));
    }

    #[test]
    fn export_tag_crosses_blank_and_plain_comment_lines() {
        let code = "#' @export\n\n# explanatory comment\n\nfoo <- 1\n";
        let ex = explicit(code).expect("blank/comment-only lines stay in the export region");
        assert_eq!(ex.members, ["foo"].into_iter().map(String::from).collect());
    }
}
