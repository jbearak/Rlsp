//! Static support for the `{import}` package's selective-import surface.
//!
//! Detection and path resolution live here because `{import}` has call shapes
//! and script-module semantics distinct from `{box}`. Once resolved, calls are
//! lowered into the shared [`crate::selective_import`] request model.

pub mod detect;
pub mod path;
pub mod resolve;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;

use crate::selective_import::{
    AttachBinding, ImportDestination, ImportProvenance, ImportSource, LocalModuleDialect,
    LocalModuleIdentity, SelectiveImportRequest,
};

/// Persisted resolution of a literal script module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalScriptResolution {
    Resolved(Url),
    CaseMismatch {
        expected: std::path::PathBuf,
        found: std::path::PathBuf,
        /// `true` when the typed path was missing and Raven found one unique
        /// case-insensitive match; `false` when the host filesystem itself
        /// accepted the wrong case.
        case_sensitive_fs: bool,
    },
    Missing,
}

/// Static source operand accepted by phase-one `{import}` support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportSpec {
    Package(String),
    LocalScript {
        path: String,
        directory: Option<String>,
    },
}

/// One statically supported `import::from`, `import::here`, or `import::into`
/// call, stored in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCall {
    pub spec: ImportSpec,
    #[serde(default)]
    pub local_resolution: Option<LocalScriptResolution>,
    pub attach: Vec<AttachBinding>,
    pub destination: ImportDestination,
    #[serde(default)]
    pub excluded_exports: BTreeSet<String>,
    pub line: u32,
    pub column: u32,
    pub end_column: u32,
    pub source_line: u32,
    pub source_column: u32,
    pub source_end_column: u32,
    #[serde(default)]
    pub function_scoped: bool,
}

impl ImportCall {
    pub fn resolved_source(&self) -> Option<ImportSource> {
        match (&self.spec, &self.local_resolution) {
            (ImportSpec::Package(package), _) => Some(ImportSource::Package(package.clone())),
            (ImportSpec::LocalScript { .. }, Some(LocalScriptResolution::Resolved(uri))) => {
                Some(ImportSource::LocalModule(LocalModuleIdentity::new(
                    uri.clone(),
                    LocalModuleDialect::ImportPackage,
                )))
            }
            (ImportSpec::LocalScript { .. }, _) => None,
        }
    }

    pub fn lower(&self, importing_uri: &Url) -> Option<SelectiveImportRequest> {
        let source = self.resolved_source()?;
        let mut excluded_exports = self.excluded_exports.clone();
        if matches!(&source, ImportSource::LocalModule(_)) {
            excluded_exports.insert(".packageName".to_string());
            excluded_exports.insert("__last_modified__".to_string());
        }
        Some(SelectiveImportRequest {
            source,
            namespace: None,
            attach: self.attach.clone(),
            destination: self.destination.clone(),
            excluded_exports,
            wildcard_skips_explicit_exports: true,
            function_scoped: self.function_scoped,
            provenance: ImportProvenance {
                uri: importing_uri.clone(),
                line: self.line,
                column: self.column,
                end_column: self.end_column.max(self.column),
            },
        })
    }

    pub fn package_name(&self) -> Option<&str> {
        match &self.spec {
            ImportSpec::Package(package) => Some(package),
            ImportSpec::LocalScript { .. } => None,
        }
    }
}
