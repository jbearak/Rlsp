//! `{import}` script-module export policy.
//!
//! A script module exposes its partial live private top-level environment. It
//! does not consult box markers, includes dotted names, and includes top-level
//! current-environment selective imports. Synthetic bookkeeping names are
//! filtered. `Partial` completeness avoids false missing-member diagnostics for
//! bindings Raven cannot statically observe.

use std::collections::BTreeSet;

use crate::selective_import::{ExportCompleteness, ExportSet};

pub(crate) fn own_live_exports(artifacts: &crate::cross_file::scope::ScopeArtifacts) -> ExportSet {
    let members: BTreeSet<String> = crate::cross_file::scope::live_top_level_exports(artifacts)
        .into_iter()
        .filter(|name| name != ".packageName" && name != "__last_modified__")
        .collect();
    ExportSet {
        members,
        completeness: ExportCompleteness::Partial,
        known_absent_prefixes: BTreeSet::new(),
    }
}
