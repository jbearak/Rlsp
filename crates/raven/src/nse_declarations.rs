//! Package-declared NSE policies (`inst/raven/nse.toml`).
//!
//! [`crate::nse`] carries Raven's *curated* policy table: the common,
//! slow-moving NSE surface of widely-used packages. That table can never cover
//! every package, and it should not try — a policy belongs next to the code it
//! describes, where it stays correct as signatures change.
//!
//! This module reads the sidecar a package can ship to declare its own policy.
//! A package puts the file at `inst/raven/nse.toml`; `R CMD INSTALL` copies
//! `inst/` to the package root, so Raven finds it at
//! `<libpath>/<pkg>/raven/nse.toml` — the same directory it already reads
//! `NAMESPACE` and `DESCRIPTION` from, so discovery costs one extra `read` on
//! the existing package-load path.
//!
//! ```toml
//! schema = 1
//!
//! [[function]]
//! name = "gen"
//! formals = ["data", "variable", "values", "where"]
//! captured = ["variable", "values", "where"]
//! ```
//!
//! Declared policies are strictly *additive*: they are consulted only after the
//! built-in table misses, so a package cannot weaken Raven's own modeling of
//! `dplyr::filter` by shipping a file that claims otherwise. They are also
//! intersected with the package's export set at load time, so a declaration for
//! an internal helper is dropped rather than applied to an unrelated call.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::nse::ArgPolicy;

/// Sidecar path relative to the installed package directory.
pub(crate) const NSE_DECLARATION_PATH: &[&str] = &["raven", "nse.toml"];

/// Schema version this build understands. A file declaring a *newer* major
/// schema is ignored wholesale rather than partially applied: a future schema
/// may change the meaning of fields this build would otherwise misread.
const SUPPORTED_SCHEMA: u32 = 1;

/// Largest sidecar Raven will read. The file is a hand-written policy table for
/// one package's exports; anything past this is not that, and the load path
/// runs during diagnostics.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// Largest number of declarations honored from one file. Guards the same path
/// against a pathological (or generated) table.
const MAX_ENTRIES: usize = 2_000;

/// Parsed policies for one package: exported name → policy.
pub(crate) type DeclaredPolicies = HashMap<String, ArgPolicy>;

#[derive(Debug, Deserialize)]
struct DeclarationFile {
    #[serde(default)]
    schema: Option<u32>,
    #[serde(default, rename = "function")]
    functions: Vec<FunctionDeclaration>,
}

#[derive(Debug, Deserialize)]
struct FunctionDeclaration {
    name: String,
    /// `"per-formal"` (default), `"whole-call"`, or `"named-arguments"`.
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    formals: Vec<String>,
    #[serde(default)]
    captured: Vec<String>,
    #[serde(default)]
    captured_dots: Option<bool>,
}

/// Read and parse `<pkg_dir>/raven/nse.toml`, keeping only declarations for
/// names in `exports`.
///
/// Best-effort by design: a missing file is the overwhelmingly common case, and
/// a malformed one yields an empty map rather than an error. A package's
/// tooling metadata must never be able to fail a package load — the worst
/// outcome of a bad sidecar is the false positives the user had before it.
pub(crate) fn load_declared_policies(
    pkg_dir: &Path,
    exports: &HashSet<String>,
) -> DeclaredPolicies {
    let mut path = pkg_dir.to_path_buf();
    for segment in NSE_DECLARATION_PATH {
        path.push(segment);
    }

    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {}
        _ => return DeclaredPolicies::new(),
    }
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return DeclaredPolicies::new();
    };
    parse_declarations(&contents, exports)
}

/// The pure half of [`load_declared_policies`], split out so the schema rules
/// are testable without a filesystem.
pub(crate) fn parse_declarations(contents: &str, exports: &HashSet<String>) -> DeclaredPolicies {
    let Ok(file) = toml::from_str::<DeclarationFile>(contents) else {
        return DeclaredPolicies::new();
    };
    // An absent `schema` is treated as v1 so an early adopter's file keeps
    // working; a newer one is ignored wholesale (see `SUPPORTED_SCHEMA`).
    if file.schema.unwrap_or(SUPPORTED_SCHEMA) > SUPPORTED_SCHEMA {
        return DeclaredPolicies::new();
    }

    let mut policies = DeclaredPolicies::new();
    for declaration in file.functions.into_iter().take(MAX_ENTRIES) {
        if !exports.contains(&declaration.name) {
            continue;
        }
        if let Some(policy) = declaration_policy(&declaration) {
            // Last declaration wins, matching the `# raven: nse` directive's
            // most-recent-wins rule.
            policies.insert(declaration.name, policy);
        }
    }
    policies
}

/// Convert one validated declaration into an [`ArgPolicy`], or `None` when it
/// is internally inconsistent (unknown policy kind, captured name that is not a
/// formal, per-formal entry that captures nothing).
fn declaration_policy(declaration: &FunctionDeclaration) -> Option<ArgPolicy> {
    match declaration.policy.as_deref().unwrap_or("per-formal") {
        "whole-call" => Some(ArgPolicy::WholeCall),
        "named-arguments" => Some(ArgPolicy::NamedArguments),
        "per-formal" => {
            let captured_dots = declaration.captured_dots.unwrap_or(false);
            // `...` is matched via `captured_dots`, not by naming it in
            // `captured`, so it is excluded from the subset check.
            let known: HashSet<&str> = declaration.formals.iter().map(String::as_str).collect();
            if !declaration
                .captured
                .iter()
                .all(|name| known.contains(name.as_str()))
            {
                return None;
            }
            // A per-formal policy that suppresses nothing is indistinguishable
            // from standard evaluation; treating it as a miss keeps the
            // built-in fallthrough in `table_verb_policy` reachable.
            if declaration.captured.is_empty() && !captured_dots {
                return None;
            }
            Some(ArgPolicy::per_formal(
                &declaration
                    .formals
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                &declaration
                    .captured
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                captured_dots,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exports(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    const GEN: &str = r#"
        schema = 1
        [[function]]
        name = "gen"
        formals = ["data", "variable", "values", "where"]
        captured = ["variable", "values", "where"]
    "#;

    #[test]
    fn parses_per_formal_declaration() {
        let policies = parse_declarations(GEN, &exports(&["gen"]));
        assert_eq!(
            policies.get("gen"),
            Some(&ArgPolicy::per_formal(
                &["data", "variable", "values", "where"],
                &["variable", "values", "where"],
                false
            ))
        );
    }

    #[test]
    fn drops_declarations_for_names_the_package_does_not_export() {
        assert!(parse_declarations(GEN, &exports(&["tab"])).is_empty());
    }

    #[test]
    fn ignores_a_newer_schema_wholesale() {
        let newer = GEN.replace("schema = 1", "schema = 2");
        assert!(parse_declarations(&newer, &exports(&["gen"])).is_empty());
    }

    #[test]
    fn treats_a_missing_schema_as_v1() {
        let unversioned = GEN.replace("schema = 1", "");
        assert!(!parse_declarations(&unversioned, &exports(&["gen"])).is_empty());
    }

    #[test]
    fn rejects_a_captured_name_that_is_not_a_formal() {
        let bad = GEN.replace(
            r#"captured = ["variable", "values", "where"]"#,
            r#"captured = ["variable", "values", "typo"]"#,
        );
        assert!(parse_declarations(&bad, &exports(&["gen"])).is_empty());
    }

    #[test]
    fn rejects_a_per_formal_declaration_that_captures_nothing() {
        let empty = GEN.replace(
            r#"captured = ["variable", "values", "where"]"#,
            "captured = []",
        );
        assert!(parse_declarations(&empty, &exports(&["gen"])).is_empty());
    }

    #[test]
    fn accepts_captured_dots_with_no_named_captures() {
        let dots = r#"
            [[function]]
            name = "select_like"
            formals = ["data", "..."]
            captured_dots = true
        "#;
        assert_eq!(
            parse_declarations(dots, &exports(&["select_like"])).get("select_like"),
            Some(&ArgPolicy::per_formal(&["data", "..."], &[], true))
        );
    }

    #[test]
    fn parses_whole_call_and_named_arguments_kinds() {
        let kinds = r#"
            [[function]]
            name = "mapping"
            policy = "whole-call"
            [[function]]
            name = "plan"
            policy = "named-arguments"
        "#;
        let policies = parse_declarations(kinds, &exports(&["mapping", "plan"]));
        assert_eq!(policies.get("mapping"), Some(&ArgPolicy::WholeCall));
        assert_eq!(policies.get("plan"), Some(&ArgPolicy::NamedArguments));
    }

    #[test]
    fn rejects_an_unknown_policy_kind() {
        let unknown = r#"
            [[function]]
            name = "gen"
            policy = "magic"
        "#;
        assert!(parse_declarations(unknown, &exports(&["gen"])).is_empty());
    }

    #[test]
    fn malformed_toml_yields_no_policies_rather_than_failing() {
        assert!(parse_declarations("[[function]\nname =", &exports(&["gen"])).is_empty());
    }

    #[test]
    fn a_missing_sidecar_yields_no_policies() {
        let dir = std::env::temp_dir().join("raven-nse-decl-absent");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_declared_policies(&dir, &exports(&["gen"])).is_empty());
    }
}
