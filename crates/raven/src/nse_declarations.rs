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
//!
//! # `[` on this package's containers is data-masking, like data.table's.
//! [subset]
//! constructors = ["dibble", "as_dibble"]
//! converters = ["set_dibble"]
//! ```
//!
//! Besides per-function policies, the optional `[subset]` table declares that
//! the package's `[` method quotes its arguments the way `[.data.table` does
//! (see [`DeclaredSubset`]). Its presence alone puts the package in the
//! bracket-NSE set the undefined-variable collector consults; `constructors`
//! and `converters` additionally let the collector classify specific objects.
//!
//! The file is parsed without `deny_unknown_fields`, so a Raven that predates a
//! key ignores it rather than rejecting the file — which is what lets `[subset]`
//! ship under `schema = 1`.
//!
//! Declared policies are strictly *additive*: they are consulted only after the
//! built-in table misses, so a package cannot weaken Raven's own modeling of
//! `dplyr::filter` by shipping a file that claims otherwise. They are also
//! intersected with the package's export set at load time, so a declaration for
//! an internal helper is dropped rather than applied to an unrelated call.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

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

/// A package's `[subset]` declaration: its `[` method is data-masking
/// (whole-call NSE), like data.table's `[.data.table`.
///
/// The declaration's *presence* is the primary fact — with the package in
/// play, an unresolved object's `[` indices are suppressed exactly as they are
/// when data.table is in play. The two lists refine that for objects the
/// collector can trace to a definition:
///
/// - `constructors`: exported functions whose return value is such a container
///   (`x <- pkg::ctor(...)` or bare `ctor(...)` classifies `x`), the analogue
///   of `data.table()` / `as.data.table()` / `fread()`.
/// - `converters`: exported by-reference converters whose first positional
///   argument becomes such a container from that call onward, the analogue of
///   `setDT(x)`.
///
/// Both are intersected with the export set at load time, like `[[function]]`
/// entries. Either may be empty; the declaration is still meaningful.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DeclaredSubset {
    pub(crate) constructors: HashSet<String>,
    pub(crate) converters: HashSet<String>,
}

/// Everything one sidecar declares.
#[derive(Debug, Default)]
pub(crate) struct Declarations {
    pub(crate) policies: DeclaredPolicies,
    /// `Some` iff the file carries a `[subset]` table (possibly empty). `Arc`
    /// so the per-diagnostic-pass snapshot in `handlers.rs` shares it rather
    /// than cloning the lists.
    pub(crate) subset: Option<Arc<DeclaredSubset>>,
}

impl Declarations {
    pub(crate) fn is_empty(&self) -> bool {
        self.policies.is_empty() && self.subset.is_none()
    }
}

#[derive(Debug, Deserialize)]
struct DeclarationFile {
    #[serde(default)]
    schema: Option<u32>,
    #[serde(default, rename = "function")]
    functions: Vec<FunctionDeclaration>,
    #[serde(default)]
    subset: Option<SubsetDeclaration>,
}

/// Raw `[subset]` table. A type error here (e.g. `constructors = 5`) fails
/// deserialization of the whole file, the same as a type error in any
/// `[[function]]` entry: the file is treated as absent rather than partially
/// applied.
#[derive(Debug, Default, Deserialize)]
struct SubsetDeclaration {
    #[serde(default)]
    constructors: Vec<String>,
    #[serde(default)]
    converters: Vec<String>,
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
/// a malformed one yields empty [`Declarations`] rather than an error. A package's
/// tooling metadata must never be able to fail a package load — the worst
/// outcome of a bad sidecar is the false positives the user had before it.
pub(crate) fn load_declared_policies(pkg_dir: &Path, exports: &HashSet<String>) -> Declarations {
    let mut path = pkg_dir.to_path_buf();
    for segment in NSE_DECLARATION_PATH {
        path.push(segment);
    }

    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {}
        _ => return Declarations::default(),
    }
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Declarations::default();
    };
    parse_declarations(&contents, exports)
}

/// The pure half of [`load_declared_policies`], split out so the schema rules
/// are testable without a filesystem.
pub(crate) fn parse_declarations(contents: &str, exports: &HashSet<String>) -> Declarations {
    let Ok(file) = toml::from_str::<DeclarationFile>(contents) else {
        return Declarations::default();
    };
    // An absent `schema` is treated as v1 so an early adopter's file keeps
    // working; a newer one is ignored wholesale (see `SUPPORTED_SCHEMA`).
    if file.schema.unwrap_or(SUPPORTED_SCHEMA) > SUPPORTED_SCHEMA {
        return Declarations::default();
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
    let subset = file.subset.map(|subset| {
        // Same export intersection and entry cap as `[[function]]`: a name the
        // package does not export is dropped, never applied to a stranger.
        let exported = |names: Vec<String>| -> HashSet<String> {
            names
                .into_iter()
                .take(MAX_ENTRIES)
                .filter(|name| exports.contains(name))
                .collect()
        };
        Arc::new(DeclaredSubset {
            constructors: exported(subset.constructors),
            converters: exported(subset.converters),
        })
    });
    Declarations { policies, subset }
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
            // R allows at most one `...`, and `per_formal_mask` stops at the
            // first one, so a second is unreachable. Reject rather than carry
            // a formal list that cannot describe a real signature.
            if declaration.formals.iter().filter(|f| *f == "...").count() > 1 {
                return None;
            }
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
        let policies = parse_declarations(GEN, &exports(&["gen"])).policies;
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
            parse_declarations(dots, &exports(&["select_like"]))
                .policies
                .get("select_like"),
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
        let policies = parse_declarations(kinds, &exports(&["mapping", "plan"])).policies;
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

    #[test]
    fn rejects_more_than_one_dots_formal() {
        let two_dots = r#"
            [[function]]
            name = "gen"
            formals = ["data", "...", "..."]
            captured_dots = true
        "#;
        assert!(parse_declarations(two_dots, &exports(&["gen"])).is_empty());
    }

    #[test]
    fn a_captured_dots_name_is_inert_rather_than_rejected() {
        // `per_formal_mask` skips `...` in both passes before consulting
        // `captured`, so naming it there is dead data. Pinned so a future
        // reader does not mistake it for a suppression path.
        let named_dots = r#"
            [[function]]
            name = "gen"
            formals = ["data", "..."]
            captured = ["..."]
        "#;
        assert_eq!(
            parse_declarations(named_dots, &exports(&["gen"]))
                .policies
                .get("gen"),
            Some(&ArgPolicy::per_formal(&["data", "..."], &["..."], false))
        );
    }

    // ---- `[subset]`: data.table-style `[` declarations ----

    const SUBSET: &str = r#"
        schema = 1
        [[function]]
        name = "gen"
        formals = ["data", "variable", "values", "where"]
        captured = ["variable", "values", "where"]

        [subset]
        constructors = ["dibble", "as_dibble", "internal_ctor"]
        converters = ["set_dibble", "internal_conv"]
    "#;

    fn subset_exports() -> HashSet<String> {
        exports(&["gen", "dibble", "as_dibble", "set_dibble"])
    }

    #[test]
    fn parses_subset_alongside_function_policies() {
        let declarations = parse_declarations(SUBSET, &subset_exports());
        assert_eq!(declarations.policies.len(), 1);
        let subset = declarations.subset.expect("[subset] present");
        assert_eq!(subset.constructors, exports(&["dibble", "as_dibble"]));
        assert_eq!(subset.converters, exports(&["set_dibble"]));
    }

    #[test]
    fn subset_drops_constructors_and_converters_the_package_does_not_export() {
        let subset = parse_declarations(SUBSET, &subset_exports())
            .subset
            .unwrap();
        assert!(!subset.constructors.contains("internal_ctor"));
        assert!(!subset.converters.contains("internal_conv"));
    }

    #[test]
    fn a_file_without_subset_declares_none() {
        // The pre-`[subset]` file shape is unchanged: no bracket declaration.
        let declarations = parse_declarations(GEN, &exports(&["gen"]));
        assert!(declarations.subset.is_none());
        assert!(!declarations.is_empty());
    }

    #[test]
    fn an_empty_subset_table_still_counts_as_a_declaration() {
        // `[subset]` with no lists says "my `[` is data-masking" and nothing
        // more — enough to put the package in the bracket-NSE set.
        let declarations = parse_declarations("[subset]\n", &exports(&[]));
        assert_eq!(
            declarations.subset.as_deref(),
            Some(&DeclaredSubset::default())
        );
        assert!(!declarations.is_empty());
    }

    #[test]
    fn a_subset_only_file_is_not_empty() {
        let declarations =
            parse_declarations("[subset]\nconstructors = [\"mk\"]\n", &exports(&["mk"]));
        assert!(!declarations.is_empty());
        assert!(declarations.policies.is_empty());
    }

    #[test]
    fn a_malformed_subset_table_discards_the_whole_file() {
        // A type error anywhere fails TOML deserialization, so the `[[function]]`
        // entries go with it — the file is treated as absent, never half-read.
        let bad = SUBSET.replace(
            r#"constructors = ["dibble", "as_dibble", "internal_ctor"]"#,
            "constructors = 5",
        );
        let declarations = parse_declarations(&bad, &subset_exports());
        assert!(declarations.is_empty());
    }

    #[test]
    fn a_newer_schema_discards_subset_too() {
        let newer = SUBSET.replace("schema = 1", "schema = 2");
        assert!(parse_declarations(&newer, &subset_exports()).is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored_not_rejected() {
        // Forward compatibility: a key this build does not know must not fail
        // the file, or a future addition could not ship under `schema = 1`.
        let future = r#"
            schema = 1
            future_top_level = true
            [subset]
            constructors = ["mk"]
            future_key = ["x"]
            [[function]]
            name = "gen"
            policy = "whole-call"
            future_field = 1
        "#;
        let declarations = parse_declarations(future, &exports(&["gen", "mk"]));
        assert_eq!(
            declarations.policies.get("gen"),
            Some(&ArgPolicy::WholeCall)
        );
        assert!(declarations.subset.unwrap().constructors.contains("mk"));
    }

    #[test]
    fn subset_lists_are_capped_like_function_entries() {
        let many: Vec<String> = (0..MAX_ENTRIES + 5).map(|i| format!("\"c{i}\"")).collect();
        let toml = format!("[subset]\nconstructors = [{}]\n", many.join(", "));
        let all: HashSet<String> = (0..MAX_ENTRIES + 5).map(|i| format!("c{i}")).collect();
        let subset = parse_declarations(&toml, &all).subset.unwrap();
        assert_eq!(subset.constructors.len(), MAX_ENTRIES);
    }
}
