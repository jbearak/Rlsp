//
// selective_import.rs
//
// A syntax-agnostic model of *selective imports*: pulling a bounded set of
// named members (and/or a whole namespace object) from one source — an
// installed R package or a resolved local module file — into another file's
// scope, behind an explicit privacy boundary.
//

//! Reusable selective-import abstraction.
//!
//! This module is deliberately **independent of any surface syntax**. It models
//! *what* a selective import means — a source identity, an exported-member set
//! with a completeness marker, an optional namespace alias, a list of attached
//! bindings, a privacy boundary, and provenance for editor navigation — without
//! knowing *how* the request was spelled in source code.
//!
//! The first producer of these values is the `box::use()` detector in
//! [`crate::box_use`]. It is intended to also back a future, second surface
//! syntax (issue #663) without changing any of the semantics here: a new
//! detector only needs to lower its parse into [`SelectiveImportRequest`]
//! values.
//!
//! # Request vs. resolved contribution
//!
//! There are two clearly separated stages, and the seam between them is the
//! whole point of this module:
//!
//! * A [`SelectiveImportRequest`] is *what the surface syntax asked for*: a
//!   [source identity](ImportSource), the dependency-edge identity of the call
//!   site (line/column + UTF-16 range, so the importer's typed module edge and
//!   this request agree), an optional [`NamespaceBinding`], the list of
//!   [`AttachBinding`]s, whether the call is function-scoped, and provenance.
//!   It carries **no** knowledge of the source's export set.
//! * A [`ResolvedImport`] is *what that request resolved to*: the request plus
//!   the source's [`ExportSet`] (with completeness) and per-member
//!   [`MemberProvenance`] (the definition site / private environment of each
//!   exported member), so downstream diagnostics/hover/go-to-definition/
//!   references can reuse [`crate::cross_file::ScopedSymbol`] where possible.
//!
//! [`resolve_request`] is the syntax-agnostic bridge: given a request and an
//! [`ImportEnv`] (package exports via the package library, local-module exports
//! via cross-file artifacts), it produces the [`ResolvedImport`], expanding a
//! wildcard attach against the resolved export set and never silently dropping
//! it.
//!
//! # Why this is distinct from existing scope machinery
//!
//! A selective import is intentionally **not** a
//! [`ScopeEvent::PackageLoad`](crate::cross_file::scope::ScopeEvent::PackageLoad)
//! and **not** an ordinary lending `source()` edge:
//!
//! * Unlike `PackageLoad`, a selective import never attaches a package's whole
//!   export set as bare names. It binds a *namespace object* (accessed as
//!   `alias$member`) and/or an explicitly enumerated subset of members.
//! * Unlike a lending `source()` edge, a module import does **not** merge the
//!   target file's top-level definitions into the importer's global scope, does
//!   not propagate `# raven: nse` / `# raven: func` declarations, and never
//!   participates in the backward parent-prefix walk. Only the target's
//!   *exported* names cross the boundary, and only under the names the import
//!   requests.
//!
//! Keeping this as its own type prevents those two very different lending
//! policies from being conflated. The dependency graph mirrors the distinction
//! with a dedicated
//! [`DependencyEdgeKind::SelectiveModule`](crate::cross_file::dependency::DependencyEdgeKind)
//! edge kind that is revalidation-visible but never lends.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;

/// Identity of the thing a selective import pulls members from.
///
/// This is the durable, resolution-complete identity: a package is named, and a
/// local module is identified by its already-resolved file `Url` (the raw URI,
/// preserving Raven's symlink/case identity conventions — see
/// [`crate::cross_file::dependency::DependencyGraph`] "Raw URI identity").
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ImportSource {
    /// An installed / frozen / bundled R package, by name. Member resolution
    /// and completeness come from
    /// [`PackageLibrary`](crate::package_library::PackageLibrary).
    Package(String),
    /// A local module, identified by its resolved file URI. This is the URI of
    /// the module *file* (`foo.r` or `foo/__init__.r`), never a directory.
    LocalModule(Url),
}

impl ImportSource {
    /// Fold this identity into `state` for interface hashing.
    fn hash_into<H: Hasher>(&self, state: &mut H) {
        match self {
            ImportSource::Package(name) => {
                0u8.hash(state);
                name.hash(state);
            }
            ImportSource::LocalModule(uri) => {
                1u8.hash(state);
                uri.as_str().hash(state);
            }
        }
    }

    /// The resolved local-module file URI, if this source is a local module.
    pub fn local_module_uri(&self) -> Option<&Url> {
        match self {
            ImportSource::LocalModule(uri) => Some(uri),
            ImportSource::Package(_) => None,
        }
    }
}

/// How complete a known export set is, for member-absence diagnostics.
///
/// Absence (`member X is not exported`) may be concluded only from a
/// [`Complete`](ExportCompleteness::Complete) set. This mirrors
/// [`MemberCompleteness`](crate::package_library::MemberCompleteness) — the
/// package-library equivalent — but is a separate, serde-serializable type so
/// this abstraction stays self-contained and can describe *local module*
/// exports (which the package library never sees). Callers that obtain a set
/// from the package library map its completeness onto this enum via
/// [`ExportCompleteness::from_member_completeness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
pub enum ExportCompleteness {
    /// Every exported member is known; absence is authoritative.
    Complete,
    /// Some members are known but the set may be incomplete; never
    /// absence-authoritative.
    Partial,
    /// Nothing authoritative is known; never absence-authoritative.
    #[default]
    Unknown,
}

impl ExportCompleteness {
    /// Map the package library's completeness onto this abstraction's.
    pub fn from_member_completeness(mc: crate::package_library::MemberCompleteness) -> Self {
        use crate::package_library::MemberCompleteness;
        match mc {
            MemberCompleteness::Complete => ExportCompleteness::Complete,
            MemberCompleteness::Partial => ExportCompleteness::Partial,
            MemberCompleteness::Unknown => ExportCompleteness::Unknown,
        }
    }

    /// Whether an absent-member conclusion may be drawn from this completeness.
    pub fn is_absence_authoritative(self) -> bool {
        matches!(self, ExportCompleteness::Complete)
    }

    /// The weaker (less authoritative) of two completeness markers. Used when a
    /// resolved export set unions members drawn from several sources (e.g. a
    /// wildcard re-export): the union is only as authoritative as its least
    /// authoritative contributor.
    pub fn min(self, other: Self) -> Self {
        use ExportCompleteness::*;
        match (self, other) {
            (Complete, Complete) => Complete,
            (Unknown, _) | (_, Unknown) => Unknown,
            _ => Partial,
        }
    }
}

/// The set of members a source makes visible across the privacy boundary,
/// together with how complete that knowledge is.
///
/// The boundary is the whole point: members **not** in `members` are private to
/// the source and must not resolve through the import, even when they are
/// live top-level names in the module file. Transitive imports of the source
/// are private too unless the source explicitly re-exports them (the export
/// parser records only re-exported names here).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSet {
    /// Exported member names, sorted and de-duplicated by `BTreeSet`.
    pub members: BTreeSet<String>,
    /// Completeness of `members`.
    pub completeness: ExportCompleteness,
    /// Prefixes that are authoritatively private even when the overall set is
    /// partial. This lets a selective-import implementation preserve categorical
    /// privacy rules (for example, legacy `{box}` modules never export dot-names)
    /// without claiming that every dynamically-created non-dot name is known.
    #[serde(default)]
    pub known_absent_prefixes: BTreeSet<String>,
}

impl ExportSet {
    /// A `Complete` export set from an iterator of names.
    pub fn complete<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            members: names.into_iter().map(Into::into).collect(),
            completeness: ExportCompleteness::Complete,
            known_absent_prefixes: BTreeSet::new(),
        }
    }

    /// An empty, `Unknown` export set — the fail-closed value for a source that
    /// could not be resolved. Never absence-authoritative.
    pub fn unresolved() -> Self {
        Self {
            members: BTreeSet::new(),
            completeness: ExportCompleteness::Unknown,
            known_absent_prefixes: BTreeSet::new(),
        }
    }

    /// Whether `name` is visible across the boundary.
    pub fn exports(&self, name: &str) -> bool {
        self.members.contains(name)
    }

    /// Classify a member lookup against this set:
    ///
    /// * present → `Some(true)`
    /// * provably absent (only when `Complete`) → `Some(false)`
    /// * not provable (Partial/Unknown and not present) → `None`
    pub fn membership(&self, name: &str) -> Option<bool> {
        if self.members.contains(name) {
            Some(true)
        } else if self.completeness.is_absence_authoritative()
            || self
                .known_absent_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            Some(false)
        } else {
            None
        }
    }

    /// Fold `other`'s members into `self`, weakening completeness to the least
    /// authoritative of the two (see [`ExportCompleteness::min`]). Known-absence
    /// prefixes survive only when both contributions prove the prefix absent;
    /// a complete contribution proves absence for every name it does not add.
    pub fn union_with(&mut self, other: &ExportSet) {
        let self_complete = self.completeness.is_absence_authoritative();
        let other_complete = other.completeness.is_absence_authoritative();
        let known_absent_prefixes = match (self_complete, other_complete) {
            (true, true) => BTreeSet::new(),
            (true, false) => other.known_absent_prefixes.clone(),
            (false, true) => self.known_absent_prefixes.clone(),
            (false, false) => intersect_prefix_constraints(
                &self.known_absent_prefixes,
                &other.known_absent_prefixes,
            ),
        };
        self.members.extend(other.members.iter().cloned());
        self.completeness = self.completeness.min(other.completeness);
        self.known_absent_prefixes = known_absent_prefixes;
    }

    /// Fold into `state` for interface hashing. Order-stable because
    /// `members` is a `BTreeSet`.
    fn hash_into<H: Hasher>(&self, state: &mut H) {
        self.completeness.hash(state);
        state.write_usize(self.members.len());
        for m in &self.members {
            m.hash(state);
        }
        state.write_usize(self.known_absent_prefixes.len());
        for prefix in &self.known_absent_prefixes {
            prefix.hash(state);
        }
    }
}

/// Intersect two sets of prefix-language constraints. The overlap of prefixes
/// `a` and `b` is the more specific one when either starts with the other; disjoint
/// prefixes have no shared known-absent names.
fn intersect_prefix_constraints(
    left: &BTreeSet<String>,
    right: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    for a in left {
        for b in right {
            if a.starts_with(b) {
                result.insert(a.clone());
            } else if b.starts_with(a) {
                result.insert(b.clone());
            }
        }
    }
    result
}

/// The namespace binding introduced by an import: the local name bound to the
/// whole source object, accessed member-wise as `alias$member`.
///
/// Absent (`None` at the [`SelectiveImportRequest`] level) for attach-only
/// imports that bind no namespace object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceBinding {
    /// Local name bound to the source object.
    pub alias: String,
}

/// One attached-member binding: a name brought directly into the importer's
/// scope (no `alias$` qualification needed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachBinding {
    /// `spec[name]` — bind local `name` to the source's exported `name`.
    Named(String),
    /// `spec[local = exported]` — bind local `local` to the source's exported
    /// `exported`.
    Renamed {
        /// Local name introduced into the importer's scope.
        local: String,
        /// Exported member name in the source.
        exported: String,
    },
    /// `spec[...]` — attach every exported member under its own name.
    Wildcard,
}

impl AttachBinding {
    /// The local name this binding introduces, or `None` for a wildcard (which
    /// introduces every export and so has no single local name).
    pub fn local_name(&self) -> Option<&str> {
        match self {
            AttachBinding::Named(n) => Some(n),
            AttachBinding::Renamed { local, .. } => Some(local),
            AttachBinding::Wildcard => None,
        }
    }

    /// The source-side exported name this binding reads, or `None` for a
    /// wildcard.
    pub fn exported_name(&self) -> Option<&str> {
        match self {
            AttachBinding::Named(n) => Some(n),
            AttachBinding::Renamed { exported, .. } => Some(exported),
            AttachBinding::Wildcard => None,
        }
    }

    fn hash_into<H: Hasher>(&self, state: &mut H) {
        match self {
            AttachBinding::Named(n) => {
                0u8.hash(state);
                n.hash(state);
            }
            AttachBinding::Renamed { local, exported } => {
                1u8.hash(state);
                local.hash(state);
                exported.hash(state);
            }
            AttachBinding::Wildcard => 2u8.hash(state),
        }
    }
}

/// Where an import was written, for go-to-definition / references and
/// diagnostic anchoring. All positions are 0-based; columns are UTF-16
/// offsets, matching the rest of the LSP state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportProvenance {
    /// The importing file's URI.
    pub uri: Url,
    /// 0-based line of the spec.
    pub line: u32,
    /// 0-based UTF-16 start column of the spec.
    pub column: u32,
    /// 0-based UTF-16 column one past the end of the spec token, for
    /// navigation ranges and diagnostics highlighting.
    #[serde(default)]
    pub end_column: u32,
}

impl ImportProvenance {
    /// The (line, column) call-site identity this import mints its typed module
    /// dependency edge at (see
    /// [`DependencyEdgeKind::SelectiveModule`](crate::cross_file::dependency::DependencyEdgeKind)).
    /// Keeping this on the provenance ties the request to the exact graph edge.
    pub fn call_site(&self) -> (u32, u32) {
        (self.line, self.column)
    }
}

/// Definition site of one exported member, for editor navigation.
///
/// This is the "private environment" location the member is really defined in
/// — a position inside the source module file (or a synthetic `package:` URI
/// for a package member). Downstream features reuse it to build a
/// [`crate::cross_file::ScopedSymbol`] pointing at the true definition rather
/// than at the import site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberProvenance {
    /// URI the member is defined in (a module file, or a `package:<name>` URI).
    pub uri: Url,
    /// 0-based definition line (0 for a synthetic package member).
    pub line: u32,
    /// 0-based UTF-16 definition column (0 for a synthetic package member).
    pub column: u32,
    /// 0-based UTF-16 column one past the definition token. Defaults to
    /// `column` for older serialized metadata that did not retain the token end.
    #[serde(default)]
    pub end_column: u32,
    /// Whether the defining member is a function. Retained independently of the
    /// local imported name so renamed attachments preserve signature help.
    #[serde(default)]
    pub is_function: bool,
    /// Static function signature from the defining module, when available.
    #[serde(default)]
    pub signature: Option<String>,
}

/// A *requested* selective import — the syntax-agnostic lowering of one
/// surface import argument, before the source's export set is resolved.
///
/// # Namespace vs. attach
///
/// `namespace` and `attach` are orthogonal. An import may bind a namespace
/// object, attach members, both, or (degenerately) neither. The producer is
/// responsible for the "attach-only does not bind a namespace unless an
/// explicit alias is given" rule; by the time a value reaches this type, that
/// decision is already encoded in whether `namespace` is `Some`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectiveImportRequest {
    /// The resolved source identity.
    pub source: ImportSource,
    /// Optional namespace-object binding (`alias$member` access).
    pub namespace: Option<NamespaceBinding>,
    /// Attached-member bindings.
    pub attach: Vec<AttachBinding>,
    /// Whether the import is lexically inside a function body. A
    /// function-scoped import binds only within that function; it never enters
    /// the file's top-level (cross-file-visible) scope. Producers must set this
    /// conservatively — `false` unless the enclosing function is proven.
    #[serde(default)]
    pub function_scoped: bool,
    /// Where the import was written.
    pub provenance: ImportProvenance,
}

impl SelectiveImportRequest {
    /// Fold the *interface-relevant* parts of this import into `state`.
    ///
    /// Provenance (line/column/end) is deliberately excluded: moving an import
    /// by a line does not change what it makes visible. The importing `uri` is
    /// also excluded because it is the identity of the file being hashed, not
    /// part of its outgoing interface. `function_scoped` **is** included: a
    /// top-level import contributes to the file's cross-file interface while a
    /// function-scoped one does not, so flipping it must revalidate dependents.
    pub fn hash_into<H: Hasher>(&self, state: &mut H) {
        self.source.hash_into(state);
        match &self.namespace {
            Some(ns) => {
                1u8.hash(state);
                ns.alias.hash(state);
            }
            None => 0u8.hash(state),
        }
        self.function_scoped.hash(state);
        state.write_usize(self.attach.len());
        for a in &self.attach {
            a.hash_into(state);
        }
    }

    /// Whether this import binds `local_name` as a name in the importer (either
    /// as the namespace alias or as an attached member). A wildcard attach is
    /// reported via [`Self::has_wildcard_attach`] instead.
    pub fn binds_local_name(&self, local_name: &str) -> bool {
        self.namespace
            .as_ref()
            .is_some_and(|ns| ns.alias == local_name)
            || self
                .attach
                .iter()
                .any(|a| a.local_name() == Some(local_name))
    }

    /// Whether this import attaches every export (a `spec[...]` wildcard).
    pub fn has_wildcard_attach(&self) -> bool {
        self.attach
            .iter()
            .any(|a| matches!(a, AttachBinding::Wildcard))
    }

    /// Resolve this request against `env`, producing a [`ResolvedImport`]. See
    /// [`resolve_request`].
    pub fn resolve(&self, env: &dyn ImportEnv) -> ResolvedImport {
        resolve_request(self, env)
    }
}

/// One local binding a resolved import introduces into the importer's scope,
/// with the provenance of the source-side definition it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinding {
    /// The local name introduced into the importer's scope.
    pub local: String,
    /// The source-side exported member this local reads, or `None` for the
    /// namespace-object alias (which is the whole source, not one member).
    pub exported: Option<String>,
    /// Whether this binding is the namespace-object alias (`alias$member`
    /// access) rather than an attached member.
    pub is_namespace: bool,
    /// Definition site of the member, when known.
    pub provenance: Option<MemberProvenance>,
}

/// A fully-resolved selective import: the request plus the source's resolved
/// export set and the concrete local bindings it introduces.
///
/// This is the value cross-file scope injection consumes. The
/// [`bindings`](Self::bindings) already have any wildcard attach expanded
/// against [`exports`](Self::exports), so a consumer never has to re-resolve
/// membership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    /// The originating request.
    pub request: SelectiveImportRequest,
    /// The source's resolved export set (with completeness).
    pub exports: ExportSet,
    /// The concrete local bindings this import introduces, in a stable order.
    pub bindings: Vec<ResolvedBinding>,
}

impl ResolvedImport {
    /// Whether `name` resolves to a member that the source exports. `Some(true)`
    /// when present, `Some(false)` when provably absent (Complete set only),
    /// `None` when membership cannot be decided.
    pub fn member_membership(&self, exported_name: &str) -> Option<bool> {
        self.exports.membership(exported_name)
    }
}

/// The environment a [`SelectiveImportRequest`] resolves against: it maps a
/// source identity to its (already fully-expanded, re-export-inclusive) export
/// set and, optionally, per-member provenance.
///
/// Production wires `package_exports` to the package library and
/// `module_exports` to cross-file artifacts (see
/// [`crate::box_use::resolve`]). The trait is the syntax-agnostic seam so
/// resolution logic here never needs to know about box, packages, or paths.
pub trait ImportEnv {
    /// The export set of an installed package.
    fn package_exports(&self, package: &str) -> ExportSet;
    /// The export set of a resolved local-module file, fully expanded through
    /// any re-exports. `None` when the module could not be read.
    fn module_exports(&self, uri: &Url) -> Option<ExportSet>;
    /// The definition site of an exported member of a source, if known.
    /// Defaults to `None`; implementations override for navigation support.
    fn member_provenance(&self, _source: &ImportSource, _member: &str) -> Option<MemberProvenance> {
        None
    }
}

/// Resolve a [`SelectiveImportRequest`] against an [`ImportEnv`].
///
/// The returned [`ResolvedImport`] has:
/// * the source's resolved [`ExportSet`] (a package's exports, or a local
///   module's re-export-expanded exports; [`ExportSet::unresolved`] when the
///   source cannot be read — fail-closed, never absence-authoritative), and
/// * one [`ResolvedBinding`] per introduced local name, with a wildcard attach
///   expanded to one binding per exported member (never silently dropped —
///   requirement #4). The namespace alias, if any, is the first binding.
pub fn resolve_request(request: &SelectiveImportRequest, env: &dyn ImportEnv) -> ResolvedImport {
    let exports = match &request.source {
        ImportSource::Package(name) => env.package_exports(name),
        ImportSource::LocalModule(uri) => env
            .module_exports(uri)
            .unwrap_or_else(ExportSet::unresolved),
    };

    let mut bindings: Vec<ResolvedBinding> = Vec::new();

    // The namespace alias binds the whole source object; it has no single
    // exported member and never leaks members as bare names.
    if let Some(ns) = &request.namespace {
        bindings.push(ResolvedBinding {
            local: ns.alias.clone(),
            exported: None,
            is_namespace: true,
            provenance: None,
        });
    }

    for a in &request.attach {
        match a {
            AttachBinding::Named(name) => bindings.push(ResolvedBinding {
                local: name.clone(),
                exported: Some(name.clone()),
                is_namespace: false,
                provenance: env.member_provenance(&request.source, name),
            }),
            AttachBinding::Renamed { local, exported } => bindings.push(ResolvedBinding {
                local: local.clone(),
                exported: Some(exported.clone()),
                is_namespace: false,
                provenance: env.member_provenance(&request.source, exported),
            }),
            AttachBinding::Wildcard => {
                // Expand against the resolved export set. Never dropped.
                for member in &exports.members {
                    bindings.push(ResolvedBinding {
                        local: member.clone(),
                        exported: Some(member.clone()),
                        is_namespace: false,
                        provenance: env.member_provenance(&request.source, member),
                    });
                }
            }
        }
    }

    ResolvedImport {
        request: request.clone(),
        exports,
        bindings,
    }
}

/// Fold an ordered slice of import requests plus the file's own export set into
/// one 64-bit interface hash suitable for inclusion in a file's cross-file
/// interface hash.
///
/// This is the single helper `compute_interface_hash` calls so an import- or
/// export-boundary edit in any connected file revalidates its dependents,
/// mirroring the `nse_declarations` / `formals` inclusion invariant documented
/// in `CLAUDE.md`.
pub fn interface_hash(imports: &[SelectiveImportRequest], own_exports: Option<&ExportSet>) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    hasher.write_usize(imports.len());
    for imp in imports {
        imp.hash_into(&mut hasher);
    }
    match own_exports {
        Some(exports) => {
            1u8.hash(&mut hasher);
            exports.hash_into(&mut hasher);
        }
        None => 0u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// Provenance of every exported member of a source, when known. Reserved for
/// resolvers that carry per-member definition sites; the type alias documents
/// the intended shape at the seam.
pub type MemberProvenanceMap = BTreeMap<String, MemberProvenance>;

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn provenance() -> ImportProvenance {
        ImportProvenance {
            uri: uri("file:///proj/a.R"),
            line: 0,
            column: 0,
            end_column: 5,
        }
    }

    fn request(namespace: Option<&str>, attach: Vec<AttachBinding>) -> SelectiveImportRequest {
        SelectiveImportRequest {
            source: ImportSource::Package("dplyr".into()),
            namespace: namespace.map(|a| NamespaceBinding { alias: a.into() }),
            attach,
            function_scoped: false,
            provenance: provenance(),
        }
    }

    /// A trivial env backing tests: one package, one module.
    struct TestEnv {
        pkg: ExportSet,
        module: Option<ExportSet>,
    }

    impl ImportEnv for TestEnv {
        fn package_exports(&self, _package: &str) -> ExportSet {
            self.pkg.clone()
        }
        fn module_exports(&self, _uri: &Url) -> Option<ExportSet> {
            self.module.clone()
        }
    }

    #[test]
    fn export_set_membership_respects_completeness() {
        let complete = ExportSet::complete(["foo", "bar"]);
        assert_eq!(complete.membership("foo"), Some(true));
        assert_eq!(complete.membership("missing"), Some(false));

        let partial = ExportSet {
            members: ["foo"].into_iter().map(String::from).collect(),
            completeness: ExportCompleteness::Partial,
            known_absent_prefixes: [".".to_string()].into_iter().collect(),
        };
        assert_eq!(partial.membership("foo"), Some(true));
        assert_eq!(partial.membership("missing"), None);
        assert_eq!(partial.membership(".private"), Some(false));

        let unknown = ExportSet::unresolved();
        assert_eq!(unknown.membership("anything"), None);
    }

    #[test]
    fn export_completeness_maps_and_mins() {
        use crate::package_library::MemberCompleteness;
        assert_eq!(
            ExportCompleteness::from_member_completeness(MemberCompleteness::Complete),
            ExportCompleteness::Complete
        );
        assert!(ExportCompleteness::Complete.is_absence_authoritative());
        assert!(!ExportCompleteness::Partial.is_absence_authoritative());
        assert!(!ExportCompleteness::Unknown.is_absence_authoritative());

        // union weakens completeness to the least authoritative contributor.
        assert_eq!(
            ExportCompleteness::Complete.min(ExportCompleteness::Complete),
            ExportCompleteness::Complete
        );
        assert_eq!(
            ExportCompleteness::Complete.min(ExportCompleteness::Partial),
            ExportCompleteness::Partial
        );
        assert_eq!(
            ExportCompleteness::Complete.min(ExportCompleteness::Unknown),
            ExportCompleteness::Unknown
        );
    }

    #[test]
    fn union_with_weakens_completeness() {
        let mut a = ExportSet::complete(["x"]);
        a.union_with(&ExportSet {
            members: ["y"].into_iter().map(String::from).collect(),
            completeness: ExportCompleteness::Partial,
            known_absent_prefixes: Default::default(),
        });
        assert!(a.exports("x") && a.exports("y"));
        assert_eq!(a.completeness, ExportCompleteness::Partial);
    }

    #[test]
    fn attach_binding_names() {
        assert_eq!(AttachBinding::Named("f".into()).local_name(), Some("f"));
        assert_eq!(AttachBinding::Named("f".into()).exported_name(), Some("f"));
        let renamed = AttachBinding::Renamed {
            local: "g".into(),
            exported: "f".into(),
        };
        assert_eq!(renamed.local_name(), Some("g"));
        assert_eq!(renamed.exported_name(), Some("f"));
        assert_eq!(AttachBinding::Wildcard.local_name(), None);
        assert_eq!(AttachBinding::Wildcard.exported_name(), None);
    }

    #[test]
    fn request_binds_local_names() {
        let imp = request(
            Some("dplyr"),
            vec![
                AttachBinding::Named("filter".into()),
                AttachBinding::Renamed {
                    local: "sel".into(),
                    exported: "select".into(),
                },
            ],
        );
        assert!(imp.binds_local_name("dplyr")); // namespace alias
        assert!(imp.binds_local_name("filter")); // attached under its own name
        assert!(imp.binds_local_name("sel")); // renamed local
        assert!(!imp.binds_local_name("select")); // exported name, not a local
        assert!(!imp.binds_local_name("nope"));
        assert!(!imp.has_wildcard_attach());

        let wildcard = request(None, vec![AttachBinding::Wildcard]);
        assert!(!wildcard.binds_local_name("dplyr"));
        assert!(wildcard.has_wildcard_attach());
    }

    #[test]
    fn resolve_expands_wildcard_against_export_set() {
        let env = TestEnv {
            pkg: ExportSet::complete(["filter", "select", "mutate"]),
            module: None,
        };
        // Namespace alias + wildcard attach.
        let req = request(Some("dplyr"), vec![AttachBinding::Wildcard]);
        let resolved = req.resolve(&env);
        // First binding is the namespace alias, then one per export.
        assert!(resolved.bindings[0].is_namespace);
        assert_eq!(resolved.bindings[0].local, "dplyr");
        let attached: BTreeSet<_> = resolved
            .bindings
            .iter()
            .filter(|b| !b.is_namespace)
            .map(|b| b.local.clone())
            .collect();
        assert_eq!(
            attached,
            ["filter", "select", "mutate"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        assert_eq!(resolved.member_membership("filter"), Some(true));
        assert_eq!(resolved.member_membership("nope"), Some(false));
    }

    #[test]
    fn resolve_named_and_renamed_are_static() {
        // Even with an unresolved export set, named/renamed locals bind.
        let env = TestEnv {
            pkg: ExportSet::unresolved(),
            module: None,
        };
        let req = request(
            None,
            vec![
                AttachBinding::Named("filter".into()),
                AttachBinding::Renamed {
                    local: "sel".into(),
                    exported: "select".into(),
                },
            ],
        );
        let resolved = req.resolve(&env);
        let locals: BTreeSet<_> = resolved.bindings.iter().map(|b| b.local.clone()).collect();
        assert_eq!(
            locals,
            ["filter", "sel"].into_iter().map(String::from).collect()
        );
        // Unresolved set → membership undecidable.
        assert_eq!(resolved.member_membership("select"), None);
    }

    #[test]
    fn resolve_unresolved_local_module_fails_closed() {
        let env = TestEnv {
            pkg: ExportSet::complete(["x"]),
            module: None, // module unreadable
        };
        let req = SelectiveImportRequest {
            source: ImportSource::LocalModule(uri("file:///proj/mod.r")),
            namespace: Some(NamespaceBinding { alias: "m".into() }),
            attach: vec![AttachBinding::Wildcard],
            function_scoped: false,
            provenance: provenance(),
        };
        let resolved = req.resolve(&env);
        // Namespace alias still binds; wildcard expands to nothing (fail-closed);
        // membership undecidable.
        assert!(resolved.bindings.iter().any(|b| b.is_namespace));
        assert_eq!(
            resolved.bindings.iter().filter(|b| !b.is_namespace).count(),
            0
        );
        assert_eq!(resolved.member_membership("anything"), None);
    }

    #[test]
    fn interface_hash_changes_when_binding_changes() {
        let base = request(Some("dplyr"), vec![AttachBinding::Named("filter".into())]);
        let h0 = interface_hash(std::slice::from_ref(&base), None);

        // Same import, only provenance moved → hash unchanged.
        let mut moved = base.clone();
        moved.provenance.line = 42;
        moved.provenance.end_column = 999;
        assert_eq!(interface_hash(std::slice::from_ref(&moved), None), h0);

        // Attach a different member → hash changes.
        let mut changed = base.clone();
        changed.attach = vec![AttachBinding::Named("select".into())];
        assert_ne!(interface_hash(std::slice::from_ref(&changed), None), h0);

        // Flipping function_scoped changes the interface.
        let mut fscoped = base.clone();
        fscoped.function_scoped = true;
        assert_ne!(interface_hash(std::slice::from_ref(&fscoped), None), h0);

        // Own-exports contribution changes the hash.
        let exports = ExportSet::complete(["a", "b"]);
        assert_ne!(
            interface_hash(std::slice::from_ref(&base), Some(&exports)),
            h0
        );
    }

    #[test]
    fn import_source_round_trips_through_serde() {
        for src in [
            ImportSource::Package("dplyr".into()),
            ImportSource::LocalModule(uri("file:///proj/mod/foo.r")),
        ] {
            let json = serde_json::to_string(&src).unwrap();
            let back: ImportSource = serde_json::from_str(&json).unwrap();
            assert_eq!(src, back);
        }
    }
}
