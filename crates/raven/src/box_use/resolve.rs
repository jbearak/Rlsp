//
// box_use/resolve.rs
//
// Resolve a box module's *effective* export set, expanding re-exports.
//

//! Box module export-set resolution.
//!
//! [`parse_box_exports`](super::parse_box_exports) records a module's
//! statically-known exports plus symbolic named, renamed, and wildcard attachment
//! re-exports ([`BoxExports::reexports`](super::BoxExports::reexports)). This
//! module validates/expands them into a fully-resolved [`ExportSet`], recursing
//! through re-exports with cycle and depth bounds.
//!
//! It also bridges the box layer to the syntax-agnostic
//! [`ImportEnv`]: [`ImportResolver`] lets a
//! [`SelectiveImportRequest`](crate::selective_import::SelectiveImportRequest)
//! be resolved against box/package/artifact data without the request layer
//! knowing anything about box.
//!
//! # The [`ModuleExportEnv`] seam
//!
//! Everything the resolver needs from the outside world is behind
//! [`ModuleExportEnv`]:
//!
//! * [`box_exports`](ModuleExportEnv::box_exports) — a module's stored
//!   [`BoxExports`], or `None` for a marker-less file;
//! * [`legacy_exports`](ModuleExportEnv::legacy_exports) — the marker-less
//!   default, derived by the caller from Raven's live, function-scope- and
//!   `rm()`-aware top-level artifacts (never a parallel assignment parser —
//!   draft problem #5);
//! * [`package_exports`](ModuleExportEnv::package_exports) — an installed
//!   package's export set from the package library;
//! * [`member_provenance`](ModuleExportEnv::member_provenance) — the definition
//!   site of one exported member, for navigation.
//!
//! Local re-export identities come from the detached resolution snapshot stored
//! in each [`BoxImport`]; package re-exports go through `package_exports`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use super::{BoxAttach, BoxExports, BoxImport};
use crate::selective_import::{
    ExportSet, ImportEnv, ImportSource, LocalModuleDialect, LocalModuleIdentity, MemberProvenance,
};

/// Maximum re-export recursion depth. Re-export chains deeper than this fail
/// closed (an [`ExportSet::unresolved`] contribution), matching the
/// budget-truncation discipline used elsewhere in cross-file traversal.
const MAX_REEXPORT_DEPTH: usize = 16;

/// The outside-world data the box export resolver depends on. See the module
/// docs for the contract of each method.
pub trait ModuleExportEnv {
    /// A module file's stored [`BoxExports`], or `None` for a marker-less file.
    fn box_exports(&self, uri: &Url) -> Option<BoxExports>;

    /// The marker-less legacy default export set for a module file, derived by
    /// the caller from live top-level artifacts (dot-prefixed names excluded;
    /// [`ExportCompleteness::Partial`](crate::selective_import::ExportCompleteness::Partial)).
    fn legacy_exports(&self, uri: &Url) -> ExportSet;

    /// An installed package's export set (from the package library).
    fn package_exports(&self, package: &str) -> ExportSet;

    /// `{import}` script-module private top-level environment. Kept separate
    /// from `box_exports`/`legacy_exports` so box marker policy never leaks
    /// across dialects.
    fn import_module_exports(&self, _uri: &Url) -> ExportSet {
        ExportSet::unresolved()
    }

    /// The definition site of one exported `member` of `source`, if known.
    /// Defaults to `None`.
    fn member_provenance(&self, _source: &ImportSource, _member: &str) -> Option<MemberProvenance> {
        None
    }
}

/// Canonical Raven adapter for resolving module exports from cross-file
/// artifacts/metadata and package exports from
/// [`PackageLibrary`](crate::package_library::PackageLibrary).
///
/// The callbacks deliberately mirror the scope engine's content-provider seam:
/// open, closed/indexed, excluded, and CLI documents all use the same canonical
/// artifact store rather than a parallel module cache. Passing `None` for the
/// package library keeps local modules functional while package export sets fail
/// closed during package-metadata cold start or when package support is disabled.
type TopLevelInterface = HashMap<Arc<str>, crate::cross_file::scope::ScopedSymbol>;

pub struct ArtifactModuleExportEnv<'a> {
    get_artifacts: &'a dyn Fn(&Url) -> Option<Arc<crate::cross_file::scope::ScopeArtifacts>>,
    get_metadata: &'a dyn Fn(&Url) -> Option<Arc<crate::cross_file::types::CrossFileMetadata>>,
    package_library: Option<&'a crate::package_library::PackageLibrary>,
    top_level_interfaces: RefCell<HashMap<Url, Arc<TopLevelInterface>>>,
    package_export_sets: RefCell<HashMap<String, ExportSet>>,
    import_module_stack: RefCell<HashSet<Url>>,
}

impl<'a> ArtifactModuleExportEnv<'a> {
    pub fn new(
        get_artifacts: &'a dyn Fn(&Url) -> Option<Arc<crate::cross_file::scope::ScopeArtifacts>>,
        get_metadata: &'a dyn Fn(&Url) -> Option<Arc<crate::cross_file::types::CrossFileMetadata>>,
        package_library: Option<&'a crate::package_library::PackageLibrary>,
    ) -> Self {
        Self {
            get_artifacts,
            get_metadata,
            package_library,
            top_level_interfaces: RefCell::new(HashMap::new()),
            package_export_sets: RefCell::new(HashMap::new()),
            import_module_stack: RefCell::new(HashSet::new()),
        }
    }

    /// Return one module's canonical live top-level interface, memoized for the
    /// lifetime of this request-local environment. Export resolution often asks
    /// for both the full member set and provenance for many members; rebuilding
    /// the timeline-derived map for every member would be quadratic.
    fn top_level_interface(&self, uri: &Url) -> Option<Arc<TopLevelInterface>> {
        if let Some(interface) = self.top_level_interfaces.borrow().get(uri) {
            return Some(interface.clone());
        }
        let artifacts = (self.get_artifacts)(uri)?;
        let interface = Arc::new(crate::cross_file::scope::top_level_interface(&artifacts));
        self.top_level_interfaces
            .borrow_mut()
            .insert(uri.clone(), interface.clone());
        Some(interface)
    }

    /// Resolve the effective definition site of `member` inside a local module.
    ///
    /// Own top-level definitions and imported bindings compete in source order,
    /// matching module execution order. Following imported bindings recursively
    /// preserves navigation through named, renamed, and wildcard re-exports
    /// without making the importer's private environment visible. Cycles and
    /// over-deep chains fail closed.
    fn local_member_provenance(
        &self,
        uri: &Url,
        member: &str,
        visited: &mut HashSet<Url>,
        depth: usize,
    ) -> Option<MemberProvenance> {
        if depth > MAX_REEXPORT_DEPTH || !visited.insert(uri.clone()) {
            return None;
        }

        let own = self.top_level_interface(uri).and_then(|interface| {
            interface.get(member).map(|symbol| {
                (
                    (symbol.defined_line, symbol.defined_column),
                    MemberProvenance {
                        uri: symbol.source_uri.clone(),
                        line: symbol.defined_line,
                        column: symbol.defined_column,
                        end_column: symbol.defined_end_column,
                        is_function: symbol.kind == crate::cross_file::scope::SymbolKind::Function,
                        signature: symbol.signature.clone(),
                    },
                )
            })
        });

        let mut best = own;
        if let Some(metadata) = (self.get_metadata)(uri) {
            for import in metadata
                .box_imports
                .iter()
                .filter(|import| !import.function_scoped)
            {
                let Some(source) = import.resolved_source() else {
                    continue;
                };

                let exported = import.attach.iter().find_map(|attach| match attach {
                    super::BoxAttach::Named(name) if name == member => Some(name.as_str()),
                    super::BoxAttach::Renamed { local, exported } if local == member => {
                        Some(exported.as_str())
                    }
                    super::BoxAttach::Wildcard => Some(member),
                    super::BoxAttach::Named(_) | super::BoxAttach::Renamed { .. } => None,
                });

                let provenance = if import.effective_alias().as_deref() == Some(member) {
                    Some(MemberProvenance {
                        uri: uri.clone(),
                        line: import.line,
                        column: import.column,
                        end_column: import.end_column.max(import.column),
                        is_function: false,
                        signature: None,
                    })
                } else if let Some(exported) = exported {
                    let exports = match &source {
                        ImportSource::Package(name) => ModuleExportEnv::package_exports(self, name),
                        ImportSource::LocalModule(source) => {
                            resolve_module_export_set(&source.uri, self)
                        }
                    };
                    // Exact navigation provenance is available only for a
                    // member positively present in the source's export set.
                    // Partial/unknown absence may still bind conservatively in
                    // scope, but must never expose a private same-named top-level
                    // definition through go-to-definition or references.
                    if !exports.exports(exported) {
                        None
                    } else {
                        match &source {
                            ImportSource::Package(_) => None,
                            ImportSource::LocalModule(source) => self.local_member_provenance(
                                &source.uri,
                                exported,
                                visited,
                                depth + 1,
                            ),
                        }
                    }
                } else {
                    None
                };

                if let Some(provenance) = provenance {
                    let order = (import.line, import.column);
                    if best
                        .as_ref()
                        .is_none_or(|(best_order, _)| order >= *best_order)
                    {
                        best = Some((order, provenance));
                    }
                }
            }

            // `{import}` redirects top-level from()/into() calls into a script
            // module's private environment (the module carries `.packageName`).
            // Therefore every top-level supported call can provide member
            // provenance here, regardless of its standalone destination label.
            for import in metadata
                .import_calls
                .iter()
                .filter(|import| !import.function_scoped)
            {
                let Some(request) = import.lower(uri) else {
                    continue;
                };
                let resolved = request.resolve(&ImportResolver::new(self));
                let Some(binding) = resolved
                    .bindings
                    .iter()
                    .find(|binding| !binding.is_namespace && binding.local == member)
                else {
                    continue;
                };
                if let Some(exported) = binding.exported.as_deref()
                    && resolved.exports.membership(exported) == Some(false)
                {
                    continue;
                }
                let provenance = binding.provenance.clone();
                if let Some(provenance) = provenance {
                    let order = (import.line, import.column);
                    if best
                        .as_ref()
                        .is_none_or(|(best_order, _)| order >= *best_order)
                    {
                        best = Some((order, provenance));
                    }
                }
            }
        }

        visited.remove(uri);
        best.map(|(_, provenance)| provenance)
    }
}

impl ModuleExportEnv for ArtifactModuleExportEnv<'_> {
    fn box_exports(&self, uri: &Url) -> Option<BoxExports> {
        (self.get_metadata)(uri)?.box_exports.clone()
    }

    fn legacy_exports(&self, uri: &Url) -> ExportSet {
        let Some(interface) = self.top_level_interface(uri) else {
            return ExportSet::unresolved();
        };
        let members = interface
            .keys()
            .filter(|name| !name.starts_with('.'))
            .map(|name| name.to_string())
            .collect();
        ExportSet {
            members,
            completeness: crate::selective_import::ExportCompleteness::Partial,
            known_absent_prefixes: [".".to_string()].into_iter().collect(),
        }
    }

    fn package_exports(&self, package: &str) -> ExportSet {
        if let Some(exports) = self.package_export_sets.borrow().get(package) {
            return exports.clone();
        }
        let Some(library) = self.package_library else {
            return ExportSet::unresolved();
        };
        let snapshot = library.namespace_exports_snapshot_sync(package);
        let completeness = match snapshot.completeness {
            crate::package_library::MemberCompleteness::Complete => {
                crate::selective_import::ExportCompleteness::Complete
            }
            crate::package_library::MemberCompleteness::Partial => {
                crate::selective_import::ExportCompleteness::Partial
            }
            crate::package_library::MemberCompleteness::Unknown => {
                crate::selective_import::ExportCompleteness::Unknown
            }
        };
        let exports = ExportSet {
            members: snapshot.members.into_iter().collect(),
            completeness,
            known_absent_prefixes: Default::default(),
        };
        self.package_export_sets
            .borrow_mut()
            .insert(package.to_string(), exports.clone());
        exports
    }

    fn import_module_exports(&self, uri: &Url) -> ExportSet {
        if !self.import_module_stack.borrow_mut().insert(uri.clone()) {
            return ExportSet::unresolved();
        }
        let result = self
            .top_level_interface(uri)
            .map_or_else(ExportSet::unresolved, |_| {
                let Some(artifacts) = (self.get_artifacts)(uri) else {
                    return ExportSet::unresolved();
                };
                let mut members = crate::import_pkg::resolve::own_live_exports(&artifacts).members;
                for event in &artifacts.timeline {
                    match event {
                        crate::cross_file::scope::ScopeEvent::Def {
                            symbol,
                            function_scope: None,
                            ..
                        }
                        | crate::cross_file::scope::ScopeEvent::Declaration {
                            symbol,
                            function_scope: None,
                            ..
                        } => {
                            members.insert(symbol.name.to_string());
                        }
                        crate::cross_file::scope::ScopeEvent::Removal {
                            symbols,
                            function_scope: None,
                            ..
                        } => {
                            for name in symbols {
                                members.remove(name);
                            }
                        }
                        crate::cross_file::scope::ScopeEvent::SelectiveImport {
                            request,
                            function_scope: None,
                            ..
                        } => {
                            // When this file is loaded as an `{import}` script
                            // module, upstream redirects named from()/into()
                            // destinations into the module's private environment.
                            let resolved = request.resolve(&ImportResolver::new(self));
                            for binding in resolved.bindings {
                                if binding.is_namespace {
                                    continue;
                                }
                                if binding.exported.as_deref().is_some_and(|exported| {
                                    resolved.exports.membership(exported) == Some(false)
                                }) {
                                    continue;
                                }
                                members.insert(binding.local);
                            }
                        }
                        _ => {}
                    }
                }
                members.remove(".packageName");
                members.remove("__last_modified__");
                ExportSet {
                    members,
                    completeness: crate::selective_import::ExportCompleteness::Partial,
                    known_absent_prefixes: Default::default(),
                }
            });
        self.import_module_stack.borrow_mut().remove(uri);
        result
    }

    fn member_provenance(&self, source: &ImportSource, member: &str) -> Option<MemberProvenance> {
        let ImportSource::LocalModule(module) = source else {
            return None;
        };
        self.local_member_provenance(&module.uri, member, &mut HashSet::new(), 0)
    }
}

impl ImportEnv for ArtifactModuleExportEnv<'_> {
    fn package_exports(&self, package: &str) -> ExportSet {
        ModuleExportEnv::package_exports(self, package)
    }

    fn module_exports(&self, module: &LocalModuleIdentity) -> Option<ExportSet> {
        Some(match module.dialect {
            LocalModuleDialect::Box => resolve_module_export_set(&module.uri, self),
            LocalModuleDialect::ImportPackage => self.import_module_exports(&module.uri),
        })
    }

    fn member_provenance(&self, source: &ImportSource, member: &str) -> Option<MemberProvenance> {
        ModuleExportEnv::member_provenance(self, source, member)
    }
}

/// Resolve `uri`'s fully-expanded effective export set.
///
/// Starts a fresh cycle-guard. Marker-less modules return their
/// [`legacy_exports`](ModuleExportEnv::legacy_exports); modules with explicit
/// exports return those unioned with every expanded re-export. A re-export that
/// cannot be resolved (missing/case-mismatched local module, unsupported spec,
/// cycle, depth overflow) contributes [`ExportSet::unresolved`], which weakens
/// the union's completeness (via [`ExportSet::union_with`]) but never drops the
/// statically-known members.
pub fn resolve_module_export_set(uri: &Url, env: &dyn ModuleExportEnv) -> ExportSet {
    let mut visited = HashSet::new();
    resolve_module_inner(uri, env, &mut visited, 0)
}

fn resolve_module_inner(
    uri: &Url,
    env: &dyn ModuleExportEnv,
    visited: &mut HashSet<Url>,
    depth: usize,
) -> ExportSet {
    if depth > MAX_REEXPORT_DEPTH {
        return ExportSet::unresolved();
    }
    if !visited.insert(uri.clone()) {
        // Re-export cycle: fail closed for this node. The outer frame keeps its
        // own statically-known members.
        return ExportSet::unresolved();
    }

    let result = match env.box_exports(uri) {
        Some(box_exports) => {
            let mut set = box_exports.to_export_set();
            for reexport in &box_exports.reexports {
                let contribution = resolve_reexport_source(reexport, env, visited, depth + 1);
                set.union_with(&contribution);
            }
            set
        }
        None => env.legacy_exports(uri),
    };

    // Remove so a *diamond* (two paths reaching the same module) resolves on
    // both branches; only a true cycle re-enters an in-progress node.
    visited.remove(uri);
    result
}

/// Resolve one tagged import's attached names against its source export set.
/// Named/renamed entries contribute only when the source does not authoritatively
/// reject the member; wildcard entries contribute the whole source set and carry
/// its completeness/known-absence constraints through the union.
fn resolve_reexport_source(
    reexport: &BoxImport,
    env: &dyn ModuleExportEnv,
    visited: &mut HashSet<Url>,
    depth: usize,
) -> ExportSet {
    let source_exports = match reexport.resolved_source() {
        Some(ImportSource::Package(name)) => env.package_exports(&name),
        Some(ImportSource::LocalModule(module)) => {
            resolve_module_inner(&module.uri, env, visited, depth)
        }
        None => ExportSet::unresolved(),
    };

    let mut contribution = ExportSet::complete(std::iter::empty::<String>());
    for attach in &reexport.attach {
        match attach {
            BoxAttach::Named(name) => {
                if source_exports.membership(name) != Some(false) {
                    contribution.members.insert(name.clone());
                }
            }
            BoxAttach::Renamed { local, exported } => {
                if source_exports.membership(exported) != Some(false) {
                    contribution.members.insert(local.clone());
                }
            }
            BoxAttach::Wildcard => contribution.union_with(&source_exports),
        }
    }
    contribution
}

/// Adapts a [`ModuleExportEnv`] to the syntax-agnostic
/// [`ImportEnv`] so a
/// [`SelectiveImportRequest`](crate::selective_import::SelectiveImportRequest)
/// can be resolved. Package exports and member provenance pass straight
/// through; a local module's export set is resolved via
/// [`resolve_module_export_set`] (re-exports expanded).
pub struct ImportResolver<'a> {
    env: &'a dyn ModuleExportEnv,
}

impl<'a> ImportResolver<'a> {
    /// Wrap a [`ModuleExportEnv`].
    pub fn new(env: &'a dyn ModuleExportEnv) -> Self {
        Self { env }
    }
}

impl ImportEnv for ImportResolver<'_> {
    fn package_exports(&self, package: &str) -> ExportSet {
        self.env.package_exports(package)
    }

    fn module_exports(&self, module: &LocalModuleIdentity) -> Option<ExportSet> {
        match module.dialect {
            LocalModuleDialect::Box => Some(resolve_module_export_set(&module.uri, self.env)),
            LocalModuleDialect::ImportPackage => Some(self.env.import_module_exports(&module.uri)),
        }
    }

    fn member_provenance(&self, source: &ImportSource, member: &str) -> Option<MemberProvenance> {
        self.env.member_provenance(source, member)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::box_use::BoxSpec;
    use crate::selective_import::{
        AttachBinding, ExportCompleteness, ImportProvenance, NamespaceBinding,
        SelectiveImportRequest,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    fn box_module(uri: Url) -> ImportSource {
        ImportSource::LocalModule(LocalModuleIdentity::new(uri, LocalModuleDialect::Box))
    }

    /// In-memory env: maps module URIs to their `BoxExports` (or a legacy set),
    /// and package names to export sets.
    #[derive(Default)]
    struct MapEnv {
        box_exports: HashMap<String, BoxExports>,
        legacy: HashMap<String, ExportSet>,
        packages: HashMap<String, ExportSet>,
    }

    impl ModuleExportEnv for MapEnv {
        fn box_exports(&self, uri: &Url) -> Option<BoxExports> {
            self.box_exports.get(uri.as_str()).cloned()
        }
        fn legacy_exports(&self, uri: &Url) -> ExportSet {
            self.legacy.get(uri.as_str()).cloned().unwrap_or_else(|| {
                // Absent module → unresolved (fail closed).
                ExportSet::unresolved()
            })
        }
        fn package_exports(&self, package: &str) -> ExportSet {
            self.packages
                .get(package)
                .cloned()
                .unwrap_or_else(ExportSet::unresolved)
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn explicit(members: &[&str]) -> BoxExports {
        BoxExports {
            members: members.iter().map(|s| s.to_string()).collect(),
            mode: super::super::ExportMode::Explicit,
            reexports: vec![],
        }
    }

    #[test]
    fn explicit_exports_are_complete() {
        let mut env = MapEnv::default();
        env.box_exports
            .insert("file:///m.r".into(), explicit(&["a", "b"]));
        let set = resolve_module_export_set(&url("file:///m.r"), &env);
        assert_eq!(set.completeness, ExportCompleteness::Complete);
        assert!(set.exports("a") && set.exports("b"));
        assert_eq!(set.membership("nope"), Some(false));
    }

    #[test]
    fn marker_less_module_uses_legacy_partial() {
        let mut env = MapEnv::default();
        env.legacy.insert(
            "file:///m.r".into(),
            ExportSet {
                members: ["foo"].into_iter().map(String::from).collect(),
                completeness: ExportCompleteness::Partial,
                known_absent_prefixes: [".".to_string()].into_iter().collect(),
            },
        );
        let set = resolve_module_export_set(&url("file:///m.r"), &env);
        assert_eq!(set.completeness, ExportCompleteness::Partial);
        assert!(set.exports("foo"));
        // Partial → absence not authoritative.
        assert_eq!(set.membership("bar"), None);
    }

    #[test]
    fn package_wildcard_reexport_is_expanded() {
        // module m: `#' @export box::use(dplyr[...])`
        let mut env = MapEnv::default();
        let mut be = explicit(&["own"]);
        be.reexports.push(BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: None,
            attach: vec![super::super::BoxAttach::Wildcard],
            line: 0,
            column: 0,
            end_column: 0,
            function_scoped: false,
        });
        env.box_exports.insert("file:///m.r".into(), be);
        env.packages
            .insert("dplyr".into(), ExportSet::complete(["filter", "select"]));

        let set = resolve_module_export_set(&url("file:///m.r"), &env);
        assert!(set.exports("own"));
        assert!(set.exports("filter") && set.exports("select"));
        // dplyr's set is Complete; union stays Complete.
        assert_eq!(set.completeness, ExportCompleteness::Complete);
    }

    #[test]
    fn named_and_renamed_reexports_respect_source_privacy() {
        let mut env = MapEnv::default();
        let mut exports = explicit(&["own"]);
        exports.reexports.push(BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: None,
            attach: vec![
                BoxAttach::Named("filter".into()),
                BoxAttach::Named("private".into()),
                BoxAttach::Renamed {
                    local: "pick".into(),
                    exported: "select".into(),
                },
                BoxAttach::Renamed {
                    local: "secret".into(),
                    exported: "private".into(),
                },
            ],
            line: 0,
            column: 0,
            end_column: 0,
            function_scoped: false,
        });
        env.box_exports.insert("file:///m.r".into(), exports);
        env.packages
            .insert("dplyr".into(), ExportSet::complete(["filter", "select"]));

        let set = resolve_module_export_set(&url("file:///m.r"), &env);
        assert!(set.exports("own"));
        assert!(set.exports("filter"));
        assert!(set.exports("pick"));
        assert!(!set.exports("private"));
        assert!(!set.exports("secret"));
        assert_eq!(set.completeness, ExportCompleteness::Complete);
    }

    #[test]
    fn unresolved_reexport_weakens_completeness() {
        let mut env = MapEnv::default();
        let mut be = explicit(&["own"]);
        be.reexports.push(BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("ghost".into()),
            explicit_alias: None,
            attach: vec![super::super::BoxAttach::Wildcard],
            line: 0,
            column: 0,
            end_column: 0,
            function_scoped: false,
        });
        env.box_exports.insert("file:///m.r".into(), be);
        // `ghost` package absent → unresolved contribution.
        let set = resolve_module_export_set(&url("file:///m.r"), &env);
        assert!(set.exports("own"));
        // Completeness weakened; absence no longer authoritative.
        assert_eq!(set.completeness, ExportCompleteness::Unknown);
        assert_eq!(set.membership("anything"), None);
    }

    #[test]
    fn import_resolver_expands_wildcard_from_module() {
        let mut env = MapEnv::default();
        env.box_exports
            .insert("file:///m.r".into(), explicit(&["a", "b"]));
        let resolver = ImportResolver::new(&env);

        let request = SelectiveImportRequest {
            source: box_module(url("file:///m.r")),
            namespace: None,
            attach: vec![AttachBinding::Wildcard],
            destination: crate::selective_import::ImportDestination::CurrentEnvironment,
            excluded_exports: Default::default(),
            wildcard_skips_explicit_exports: false,
            function_scoped: false,
            provenance: ImportProvenance {
                uri: url("file:///importer.r"),
                line: 0,
                column: 0,
                end_column: 0,
            },
        };
        let resolved = request.resolve(&resolver);
        let locals: HashSet<_> = resolved.bindings.iter().map(|b| b.local.clone()).collect();
        assert_eq!(locals, ["a", "b"].into_iter().map(String::from).collect());
    }

    #[test]
    fn artifact_env_legacy_exports_filter_private_names_and_keep_exact_provenance() {
        let uri = url("file:///module.r");
        let code = "public <- 1\n.private <- 2\n";
        let document = crate::state::Document::new_with_uri(code, None, &uri);
        let metadata = Arc::new(crate::cross_file::extract_metadata(code));
        let artifacts = Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
            &uri,
            document.tree.as_ref().expect("tree"),
            code,
            Some(&metadata),
        ));
        let artifacts_for_lookup = artifacts.clone();
        let metadata_for_lookup = metadata.clone();
        let get_artifacts =
            move |target: &Url| (target == &uri).then(|| artifacts_for_lookup.clone());
        let uri = url("file:///module.r");
        let get_metadata =
            move |target: &Url| (target == &uri).then(|| metadata_for_lookup.clone());
        let env = ArtifactModuleExportEnv::new(&get_artifacts, &get_metadata, None);
        let uri = url("file:///module.r");

        let exports = env.legacy_exports(&uri);
        assert_eq!(exports.completeness, ExportCompleteness::Partial);
        assert!(exports.exports("public"));
        assert!(!exports.exports(".private"));

        let provenance =
            ModuleExportEnv::member_provenance(&env, &box_module(uri.clone()), "public")
                .expect("public provenance");
        assert_eq!(provenance.uri, uri);
        assert_eq!(provenance.line, 0);
        assert_eq!(provenance.column, 0);
        assert_eq!(provenance.end_column, 6);
    }

    #[test]
    fn artifact_env_follows_renamed_and_wildcard_reexport_provenance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base_path = dir.path().join("base.r");
        let renamed_path = dir.path().join("renamed.r");
        let wildcard_path = dir.path().join("wildcard.r");
        let base_code = "box::export(original)\noriginal <- 1\n";
        let renamed_code = "#' @export\nbox::use(./base[renamed = original])\n";
        let wildcard_code = "#' @export\nbox::use(./base[...])\n";
        std::fs::write(&base_path, base_code).expect("write base");
        std::fs::write(&renamed_path, renamed_code).expect("write renamed");
        std::fs::write(&wildcard_path, wildcard_code).expect("write wildcard");

        let mut artifacts = HashMap::new();
        let mut metadata = HashMap::new();
        for (path, code) in [
            (&base_path, base_code),
            (&renamed_path, renamed_code),
            (&wildcard_path, wildcard_code),
        ] {
            let uri = Url::from_file_path(path).expect("file uri");
            let document = crate::state::Document::new_with_uri(code, None, &uri);
            let mut meta = crate::cross_file::extract_metadata(code);
            crate::cross_file::enrich_box_import_resolutions(&mut meta, &uri);
            let meta = Arc::new(meta);
            let artifact = Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
                &uri,
                document.tree.as_ref().expect("tree"),
                code,
                Some(&meta),
            ));
            metadata.insert(uri.clone(), meta);
            artifacts.insert(uri, artifact);
        }

        let get_artifacts = |uri: &Url| artifacts.get(uri).cloned();
        let get_metadata = |uri: &Url| metadata.get(uri).cloned();
        let env = ArtifactModuleExportEnv::new(&get_artifacts, &get_metadata, None);
        let base_uri = Url::from_file_path(&base_path).expect("base uri");

        for (module_path, member) in [(&renamed_path, "renamed"), (&wildcard_path, "original")] {
            let module_uri = Url::from_file_path(module_path).expect("module uri");
            let provenance =
                ModuleExportEnv::member_provenance(&env, &box_module(module_uri), member)
                    .expect("re-export provenance");
            assert_eq!(provenance.uri, base_uri);
            assert_eq!(provenance.line, 1);
            assert_eq!(provenance.column, 0);
            assert_eq!(provenance.end_column, 8);
        }
    }

    #[test]
    fn namespace_import_binds_alias_only() {
        let mut env = MapEnv::default();
        env.box_exports
            .insert("file:///m.r".into(), explicit(&["a", "b"]));
        let resolver = ImportResolver::new(&env);
        let request = SelectiveImportRequest {
            source: box_module(url("file:///m.r")),
            namespace: Some(NamespaceBinding { alias: "m".into() }),
            attach: vec![],
            destination: crate::selective_import::ImportDestination::CurrentEnvironment,
            excluded_exports: Default::default(),
            wildcard_skips_explicit_exports: false,
            function_scoped: false,
            provenance: ImportProvenance {
                uri: url("file:///importer.r"),
                line: 0,
                column: 0,
                end_column: 0,
            },
        };
        let resolved = request.resolve(&resolver);
        // Only the namespace alias — members are NOT leaked as bare names.
        assert_eq!(resolved.bindings.len(), 1);
        assert!(resolved.bindings[0].is_namespace);
        assert_eq!(resolved.bindings[0].local, "m");
    }
}
