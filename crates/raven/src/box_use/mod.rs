//
// box_use/mod.rs
//
// The `box::use()` surface syntax for R module imports.
//
// This module and its submodules own *everything box-specific*: recognising
// `box::use()` / `box::export()` calls, parsing their module specs and attach
// lists, parsing a module's exported interface, and resolving local module
// paths. The syntax-agnostic *meaning* of an import lives in
// [`crate::selective_import`]; this layer only produces those values.
//

//! `box::use()` support (issue #662).
//!
//! [box](https://klmr.me/box/) is an R module system. A file can import from an
//! installed package or a *local module* (another R file) with:
//!
//! ```r
//! box::use(
//!   dplyr,                 # bind the `dplyr` namespace object (dplyr$filter)
//!   dr = dplyr,            # ... under an explicit alias `dr`
//!   dplyr[filter, select], # attach `filter`, `select` directly (no namespace)
//!   dplyr[f = filter],     # attach `dplyr::filter` under the local name `f`
//!   dplyr[...],            # attach every export
//!   ./helpers,             # a local module `helpers.r` / `helpers/__init__.r`
//!   ../lib/util[foo],      # attach `foo` from `../lib/util`
//! )
//! ```
//!
//! # Supported scope (v0.18.0)
//!
//! * **Static `box::use()` only** — a literal `box::use(...)` / `box:::use(...)`
//!   call. Programmatic invocation (`do.call`, aliasing `box::use`) is not
//!   recognised.
//! * **Bare name = installed package.** A **local module MUST begin with `./`
//!   or `../`.** Non-local module search paths (`foo/bar`, `options(box.path)`,
//!   `R_BOX_PATH`, remote/global modules) are **unsupported** and fail
//!   conservatively — recorded as [`BoxSpec::Unsupported`] so they neither bind
//!   nor emit misleading diagnostics.
//! * Local paths resolve **relative to the importing file's directory**, ignore
//!   `# raven: cd`, the implicit testthat working directory, and the
//!   workspace-root fallback, and omit the file extension. Resolution is
//!   **case-sensitive** (box module names are case-sensitive); a case-only
//!   mismatch is *diagnosed*, never silently corrected. Candidate order is
//!   `path.r`, `path.R`, `path/__init__.r`, `path/__init__.R` — a `.r`/`.R`
//!   file module wins over an `__init__` package module, per box.
//! * The **default namespace alias** is the final module/package component;
//!   `alias = spec` overrides it.
//! * Attach lists `spec[a, b]`, renamed `spec[local = exported]`, wildcard
//!   `spec[...]`, and combinations are supported. An **attach-only** spec does
//!   **not** bind a namespace object unless an outer `alias = spec[...]` is
//!   given.
//!
//! See `docs/cross-file.md` and `docs/limitations.md` for the full contract and
//! the list of deliberately unsupported dynamic forms.

pub mod detect;
pub mod exports;
pub mod path;
pub mod resolve;

pub use detect::detect_box_imports;
pub use exports::{ExportMode, parse_box_exports};
pub use path::{BoxResolveError, ResolvedModule};
pub use resolve::{ArtifactModuleExportEnv, ModuleExportEnv, resolve_module_export_set};

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::selective_import::{
    AttachBinding, ExportCompleteness, ExportSet, ImportDestination, ImportProvenance,
    ImportSource, LocalModuleDialect, LocalModuleIdentity, NamespaceBinding,
    SelectiveImportRequest,
};

/// Persisted outcome of resolving a local module's ordered candidate set.
///
/// Resolution is enriched during detached analysis, before metadata/artifacts are
/// committed. Locked graph, scope, diagnostics, and editor request paths consume
/// this snapshot and never touch the filesystem themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalModuleResolution {
    /// A candidate resolved exactly and is a regular file.
    Resolved(tower_lsp::lsp_types::Url),
    /// A unique case-only candidate exists and must be diagnosed, not followed.
    CaseMismatch {
        /// Literal candidate path expected from the import spelling.
        expected: std::path::PathBuf,
        /// Actual differently-cased path found on disk.
        found: std::path::PathBuf,
    },
    /// No regular-file candidate exists (or the importer has no parent directory).
    Missing,
}

/// A parsed `box::use()` module/package spec, at the surface-syntax level
/// (before local-module path resolution).
///
/// This is what [`CrossFileMetadata`](crate::cross_file::CrossFileMetadata)
/// stores. Resolving a [`BoxSpec::LocalModule`] to a concrete file URI, and
/// lowering into a [`SelectiveImportRequest`] happens later via [`path`] — mirroring
/// how [`ForwardSource`](crate::cross_file::ForwardSource) stores a raw path and
/// defers resolution to the path-resolve layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxImport {
    /// The module/package the import reads from.
    pub spec: BoxSpec,
    /// Detached filesystem-resolution snapshot for a local module. `None` means
    /// the raw syntax has not yet been enriched (or the spec is not local).
    #[serde(default)]
    pub local_resolution: Option<LocalModuleResolution>,
    /// Explicit `alias = ...` written on the argument, if any.
    #[serde(default)]
    pub explicit_alias: Option<String>,
    /// Attach-list bindings (`[a, b]`, `[local = exported]`, `[...]`).
    #[serde(default)]
    pub attach: Vec<BoxAttach>,
    /// 0-based line of the spec.
    pub line: u32,
    /// 0-based UTF-16 column of the spec.
    pub column: u32,
    /// 0-based UTF-16 column one past the end of the spec token, for
    /// diagnostics and navigation ranges. Defaults to `column` on legacy
    /// artifacts that predate the field.
    #[serde(default)]
    pub end_column: u32,
    /// Whether the `box::use()` call is lexically inside a function body. A
    /// function-scoped import binds only within that function and never enters
    /// the file's cross-file-visible top-level scope. Set conservatively: only
    /// `true` when an enclosing `function_definition` is proven.
    #[serde(default)]
    pub function_scoped: bool,
}

/// The module/package a [`BoxImport`] targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxSpec {
    /// A bare name — an installed package.
    Package(String),
    /// A local module beginning with `./` or `../`.
    ///
    /// `up_levels` counts leading `../` (so `./x` is `0`, `../x` is `1`).
    /// `components` are the path parts after the leading markers; the **last**
    /// is the module name and any earlier parts are directory segments. Never
    /// empty for a well-formed local spec.
    LocalModule {
        /// Number of parent-directory (`..`) hops before `components`.
        up_levels: usize,
        /// Path parts after the leading markers; last is the module name.
        components: Vec<String>,
    },
    /// A spec we recognise syntactically but deliberately do not support
    /// (non-local module search path such as `foo/bar`, or an otherwise
    /// malformed spec). Retained verbatim so tooling can explain the gap and so
    /// it never silently binds. Fails conservatively.
    Unsupported(String),
}

impl BoxSpec {
    /// The default namespace alias for this spec: the package name, or the last
    /// local-module component. `None` for [`BoxSpec::Unsupported`].
    pub fn default_alias(&self) -> Option<String> {
        match self {
            BoxSpec::Package(name) => Some(name.clone()),
            BoxSpec::LocalModule { components, .. } => components.last().cloned(),
            BoxSpec::Unsupported(_) => None,
        }
    }

    /// Whether this spec is a supported (package or local) target.
    pub fn is_supported(&self) -> bool {
        !matches!(self, BoxSpec::Unsupported(_))
    }
}

/// One entry in a `box::use()` attach list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxAttach {
    /// `spec[name]`.
    Named(String),
    /// `spec[local = exported]`.
    Renamed {
        /// Local name introduced into the importer.
        local: String,
        /// Source-side exported name.
        exported: String,
    },
    /// `spec[...]` — attach all exports.
    Wildcard,
}

impl BoxImport {
    /// Return the committed syntax-independent source identity without touching
    /// the filesystem. Package specs are immediate; local specs require a
    /// detached [`LocalModuleResolution::Resolved`] snapshot.
    pub fn resolved_source(&self) -> Option<ImportSource> {
        match (&self.spec, &self.local_resolution) {
            (BoxSpec::Package(package), _) => Some(ImportSource::Package(package.clone())),
            (BoxSpec::LocalModule { .. }, Some(LocalModuleResolution::Resolved(uri))) => {
                Some(ImportSource::LocalModule(LocalModuleIdentity::new(
                    uri.clone(),
                    LocalModuleDialect::Box,
                )))
            }
            (BoxSpec::LocalModule { .. }, _) | (BoxSpec::Unsupported(_), _) => None,
        }
    }

    /// Whether this import binds a namespace object.
    ///
    /// box binds a namespace object when the spec has **no attach list**, or
    /// when an explicit `alias =` is written. An attach-only spec
    /// (`pkg[a, b]`, `pkg[...]`) binds no namespace object.
    pub fn binds_namespace(&self) -> bool {
        self.explicit_alias.is_some() || self.attach.is_empty()
    }

    /// The namespace alias actually bound, or `None` for an attach-only import
    /// or an unsupported spec.
    pub fn effective_alias(&self) -> Option<String> {
        if !self.binds_namespace() {
            return None;
        }
        self.explicit_alias
            .clone()
            .or_else(|| self.spec.default_alias())
    }

    /// Lower this surface parse into a syntax-agnostic
    /// [`SelectiveImportRequest`], given the resolved source identity and the
    /// importing file's URI.
    ///
    /// The caller supplies the [`ImportSource`] (a package name, or the resolved
    /// local-module URI from
    /// [`path::resolve_local_module`]). The
    /// per-argument attach/alias structure carries through unchanged; the only
    /// thing this method adds is the importing-file identity and provenance.
    pub fn lower(
        &self,
        importing_uri: &tower_lsp::lsp_types::Url,
        source: ImportSource,
    ) -> SelectiveImportRequest {
        let namespace = self
            .effective_alias()
            .map(|alias| NamespaceBinding { alias });
        let attach = self.lowered_attach();
        SelectiveImportRequest {
            source,
            namespace,
            attach,
            destination: ImportDestination::CurrentEnvironment,
            excluded_exports: Default::default(),
            wildcard_skips_explicit_exports: false,
            function_scoped: self.function_scoped,
            provenance: ImportProvenance {
                uri: importing_uri.clone(),
                line: self.line,
                column: self.column,
                end_column: self.end_column.max(self.column),
            },
        }
    }

    /// Lower just the attach list into syntax-agnostic [`AttachBinding`]s.
    fn lowered_attach(&self) -> Vec<AttachBinding> {
        self.attach
            .iter()
            .map(|a| match a {
                BoxAttach::Named(n) => AttachBinding::Named(n.clone()),
                BoxAttach::Renamed { local, exported } => AttachBinding::Renamed {
                    local: local.clone(),
                    exported: exported.clone(),
                },
                BoxAttach::Wildcard => AttachBinding::Wildcard,
            })
            .collect()
    }
}

/// A module's exported interface, parsed from box export markers.
///
/// See [`exports::parse_box_exports`] for how this is derived. Marker-less
/// modules derive their legacy fallback from the live top-level scope.
///
/// # Symbolic re-exports
///
/// A `#' @export` tag on a `box::use()` argument re-exports what that import
/// brought in. Namespace aliases are enumerated directly into
/// [`members`](Self::members). Named, renamed, and wildcard attachments depend on
/// the imported source's effective export boundary, so the full import is stored
/// in [`reexports`](Self::reexports) and expanded at resolution time by
/// [`resolve::resolve_module_export_set`], with cycle bounds. This both preserves
/// wildcard exports and prevents named declarations from exposing a source member
/// that is authoritatively private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxExports {
    /// Statically-known exported member names (own definitions plus namespace
    /// aliases whose presence does not depend on source-member resolution).
    pub members: BTreeSet<String>,
    /// How the export set was determined.
    pub mode: ExportMode,
    /// Attachment re-exports awaiting resolution: each is the re-exported
    /// [`BoxImport`] whose named, renamed, or wildcard attachments are validated
    /// against the source export set. Empty when no tagged import attaches names.
    #[serde(default)]
    pub reexports: Vec<BoxImport>,
}

impl BoxExports {
    /// Convert the *statically-known* part to the syntax-agnostic [`ExportSet`].
    ///
    /// [`ExportMode::Explicit`] sets are `Complete` (box export markers are
    /// authoritative, so member absence can be diagnosed). A
    /// [`ExportMode::LegacyDefault`] set is `Partial`: statically-collected
    /// top-level names may miss dynamically-created bindings, so absence is
    /// **not** authoritative for a marker-less module.
    ///
    /// This does **not** expand [`reexports`](Self::reexports); that requires
    /// path/package resolution and happens in
    /// [`resolve::resolve_module_export_set`], which unions the expanded
    /// re-export sets in and weakens completeness accordingly.
    pub fn to_export_set(&self) -> ExportSet {
        let completeness = match self.mode {
            ExportMode::Explicit => ExportCompleteness::Complete,
            ExportMode::LegacyDefault => ExportCompleteness::Partial,
        };
        ExportSet {
            members: self.members.clone(),
            completeness,
            known_absent_prefixes: BTreeSet::new(),
        }
    }

    /// Whether this export interface has unresolved wildcard re-exports.
    pub fn has_reexports(&self) -> bool {
        !self.reexports.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Url;

    #[test]
    fn binds_namespace_and_effective_alias() {
        // Bare package: binds namespace under its own name.
        let bare = BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: None,
            attach: vec![],
            line: 0,
            column: 0,
            end_column: 5,
            function_scoped: false,
        };
        assert!(bare.binds_namespace());
        assert_eq!(bare.effective_alias().as_deref(), Some("dplyr"));

        // Explicit alias.
        let aliased = BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: Some("dr".into()),
            attach: vec![],
            line: 0,
            column: 0,
            end_column: 5,
            function_scoped: false,
        };
        assert_eq!(aliased.effective_alias().as_deref(), Some("dr"));

        // Attach-only: no namespace binding.
        let attach_only = BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: None,
            attach: vec![BoxAttach::Named("filter".into())],
            line: 0,
            column: 0,
            end_column: 5,
            function_scoped: false,
        };
        assert!(!attach_only.binds_namespace());
        assert_eq!(attach_only.effective_alias(), None);

        // Attach-only with explicit alias: binds namespace too.
        let attach_aliased = BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: Some("d".into()),
            attach: vec![BoxAttach::Wildcard],
            line: 0,
            column: 0,
            end_column: 5,
            function_scoped: false,
        };
        assert!(attach_aliased.binds_namespace());
        assert_eq!(attach_aliased.effective_alias().as_deref(), Some("d"));
    }

    #[test]
    fn local_module_default_alias_is_last_component() {
        let spec = BoxSpec::LocalModule {
            up_levels: 1,
            components: vec!["lib".into(), "util".into()],
        };
        assert_eq!(spec.default_alias().as_deref(), Some("util"));
        assert!(spec.is_supported());

        assert_eq!(BoxSpec::Unsupported("foo/bar".into()).default_alias(), None);
        assert!(!BoxSpec::Unsupported("foo/bar".into()).is_supported());
    }

    #[test]
    fn lower_produces_selective_import() {
        let uri = Url::parse("file:///proj/a.R").unwrap();
        let imp = BoxImport {
            local_resolution: None,
            spec: BoxSpec::Package("dplyr".into()),
            explicit_alias: None,
            attach: vec![
                BoxAttach::Named("filter".into()),
                BoxAttach::Renamed {
                    local: "sel".into(),
                    exported: "select".into(),
                },
            ],
            line: 3,
            column: 2,
            end_column: 24,
            function_scoped: false,
        };
        // Attach-list present without explicit alias → no namespace binding.
        let lowered = imp.lower(&uri, ImportSource::Package("dplyr".into()));
        assert!(lowered.namespace.is_none());
        assert_eq!(lowered.attach.len(), 2);
        assert_eq!(lowered.provenance.line, 3);
        assert_eq!(lowered.provenance.column, 2);
        assert_eq!(lowered.provenance.end_column, 24);
        assert!(!lowered.function_scoped);
    }

    #[test]
    fn box_exports_completeness_by_mode() {
        let explicit = BoxExports {
            members: ["a", "b"].into_iter().map(String::from).collect(),
            mode: ExportMode::Explicit,
            reexports: vec![],
        };
        assert_eq!(
            explicit.to_export_set().completeness,
            ExportCompleteness::Complete
        );

        let legacy = BoxExports {
            members: ["a"].into_iter().map(String::from).collect(),
            mode: ExportMode::LegacyDefault,
            reexports: vec![],
        };
        assert_eq!(
            legacy.to_export_set().completeness,
            ExportCompleteness::Partial
        );
    }

    #[test]
    fn box_import_round_trips_through_serde() {
        let imp = BoxImport {
            local_resolution: None,
            spec: BoxSpec::LocalModule {
                up_levels: 0,
                components: vec!["helpers".into()],
            },
            explicit_alias: Some("h".into()),
            attach: vec![BoxAttach::Wildcard],
            line: 1,
            column: 4,
            end_column: 20,
            function_scoped: true,
        };
        let json = serde_json::to_string(&imp).unwrap();
        let back: BoxImport = serde_json::from_str(&json).unwrap();
        assert_eq!(imp, back);
    }
}
