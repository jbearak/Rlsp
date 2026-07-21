//
// cross_file/types.rs
//
// Core types for cross-file awareness
//

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use tower_lsp::lsp_types::Url;

use super::source_detect::{LibraryCall, NamespaceReference};

/// What a `# raven: ignore` directive (the `@lsp-` forms named throughout this
/// file are permanent aliases that parse identically) on a given line targets.
///
/// A blanket directive suppresses every analyzer diagnostic on its line; a
/// code-scoped directive (`# raven: ignore[undefined-variable]`) suppresses
/// only diagnostics whose code is covered by one of the listed codes, with
/// cascading sub-kinds via [`crate::diagnostic_code::suppresses`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LineSuppression {
    /// Blanket ignore — suppresses all analyzer diagnostics on the line.
    All,
    /// Suppress only diagnostics whose code is covered by one of these
    /// (normalized, kebab-case) codes.
    Codes(Vec<String>),
}

impl LineSuppression {
    /// Does this suppression cover a diagnostic with the given code?
    ///
    /// `All` covers everything. `Codes` covers a diagnostic only when its code
    /// is known (`Some`) and one of the listed codes
    /// [`suppresses`](crate::diagnostic_code::suppresses) it.
    pub fn covers(&self, diagnostic_code: Option<&str>) -> bool {
        match self {
            LineSuppression::All => true,
            LineSuppression::Codes(codes) => match diagnostic_code {
                Some(dc) => codes
                    .iter()
                    .any(|c| crate::diagnostic_code::suppresses(c, dc)),
                None => false,
            },
        }
    }

    /// Merge another suppression into this one. `All` is absorbing; otherwise
    /// the code lists are concatenated.
    pub fn merge(&mut self, other: LineSuppression) {
        match (&mut *self, other) {
            (LineSuppression::All, _) => {}
            (slot, LineSuppression::All) => *slot = LineSuppression::All,
            (LineSuppression::Codes(existing), LineSuppression::Codes(more)) => {
                for c in more {
                    if !existing.contains(&c) {
                        existing.push(c);
                    }
                }
            }
        }
    }
}

/// A declared symbol from a `# raven: var` or `# raven: func` directive.
/// These directives allow users to declare symbols that cannot be statically
/// detected by the parser (e.g., dynamically created via eval(), assign(), load()).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredSymbol {
    /// The symbol name in CALL-SITE form (e.g. `myvar`, `my.func`). A
    /// non-syntactic name is stored backtick-wrapped (`` `my fn` `` for
    /// `# raven: func "my fn"`) so it matches the usage's `node_text`; a
    /// `pkg::` qualifier on a `# raven: func` is kept as `pkg::name`. See
    /// `callee_name_for_match` in `cross_file::directive`.
    pub name: String,
    /// 0-based line number where the directive appears
    pub line: u32,
    /// true for `# raven: func`, false for `# raven: var`
    pub is_function: bool,
    /// For `# raven: func name(a, b, c)`, the declared ordered formal names.
    /// `None` when no parameter list was written (and always `None` for
    /// variables). `Some(vec)` carries the declared formal order, used as an
    /// authoritative source for NSE positional argument matching.
    #[serde(default)]
    pub formals: Option<Vec<String>>,
}

/// The argument-evaluation scope a `# raven: nse` directive declares for a
/// callee. `WholeCall` (no parentheses) means every argument is NSE; `Formals`
/// names the captured/data-masked formals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NseScope {
    /// `# raven: nse my_func` (or empty parens `my_func()`) — suppress
    /// undefined-variable in every argument.
    WholeCall,
    /// `# raven: nse my_func(x, y)` — suppress only arguments bound to these
    /// formals. Never empty: empty parens parse as [`NseScope::WholeCall`].
    Formals(Vec<String>),
}

/// A user-declared non-standard-evaluation contract from a `# raven: nse`
/// directive (`@lsp-nse` is a permanent alias that parses identically).
///
/// Position-aware: applies only to calls on a line strictly after `line`. The
/// callee is matched per the resolution model in `resolve_call_arg_policy`:
/// an unqualified declaration matches unqualified calls; a qualified
/// declaration matches `package::name` calls and unqualified `name` calls when
/// `package` is in scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NseDeclaration {
    /// Bare callee name (the `name` of a `package::name`, or the whole name) in
    /// CALL-SITE form: a non-syntactic name is stored backtick-wrapped
    /// (`` `my fn` `` for `# raven: nse "my fn"`) so it matches the call's
    /// `node_text`. See `callee_name_for_match` in `cross_file::directive`.
    pub name: String,
    /// Package qualifier when written `package::name`, else `None`.
    pub package: Option<String>,
    /// Declared NSE scope (whole-call or named formals).
    pub scope: NseScope,
    /// 0-based line of the directive comment. Applies to calls on lines `> line`.
    pub line: u32,
}

/// An inclusive line range suppressed by a `# raven: ignore-start` …
/// `# raven: ignore-end` block (or a chunk-level suppression mapped onto the
/// chunk's line range). 0-based, `end` inclusive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuppressionRange {
    pub start: u32,
    pub end: u32,
    pub what: LineSuppression,
}

/// The flavor of a suppression directive (F2 Step 3).
///
/// `Ignore` is silent: it never warns, even when it suppressed nothing (like
/// Rust's `#[allow]` / `@ts-ignore`). `Expect` asserts that a diagnostic *will*
/// be suppressed: if it suppressed nothing, an `unused-suppression` hint is
/// emitted at the directive's line (like Rust's `#[expect]` /
/// `@ts-expect-error`). Both flavors suppress diagnostics identically; they
/// differ only in the `unused-suppression` sweep.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SuppressionFlavor {
    /// Silent suppression — never reported as unused unless the global
    /// `reportUnusedSuppressions` sweep is enabled.
    Ignore,
    /// Asserting suppression — reported as unused whenever it suppressed
    /// nothing, regardless of the global sweep.
    Expect,
}

/// One parsed suppression directive, retained for the `unused-suppression`
/// sweep (F2 Step 3).
///
/// Unlike the inline `ignored_*` maps — which are keyed by *target* line for
/// fast per-diagnostic lookup — this records the directive's own line (the
/// anchor where an `unused-suppression` hint is reported), the inclusive target
/// line range it governs, what it suppresses, and its flavor. A directive is
/// "used" iff at least one diagnostic on a covered line carries a code its
/// `what` covers; an unused `Expect` (or, under the global sweep, an unused
/// `Ignore`) produces an `unused-suppression` diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuppressionDirective {
    /// 0-based line of the directive comment itself (hint anchor).
    pub directive_line: u32,
    /// First 0-based line the directive suppresses (inclusive).
    pub target_start: u32,
    /// Last 0-based line the directive suppresses (inclusive). `u32::MAX` for a
    /// file-level directive, which covers every line.
    pub target_end: u32,
    /// What the directive suppresses (blanket or code-scoped).
    pub what: LineSuppression,
    /// `Ignore` (silent) or `Expect` (asserts a suppression occurs).
    pub flavor: SuppressionFlavor,
}

impl SuppressionDirective {
    /// Does this directive govern `line`?
    pub fn covers_line(&self, line: u32) -> bool {
        line >= self.target_start && line <= self.target_end
    }
}

/// One statically recognized package from a top-level {targets}
/// `tar_option_set(packages = ...)` declaration.
///
/// This is intentionally separate from [`LibraryCall`]: targets worker
/// packages are a file/pipeline-level contribution, not a lexical package-load
/// event. The source anchor is retained for missing-package diagnostics and
/// line-scoped suppressions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetsPackageDeclaration {
    /// Statically resolved package name.
    pub package: String,
    /// 0-based line of the package literal or call-end fallback.
    pub line: u32,
    /// 0-based UTF-16 column of the package literal or call-end fallback.
    pub column: u32,
}

/// Complete cross-file metadata for a document
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossFileMetadata {
    /// Backward directives (this file is sourced by others)
    pub sourced_by: Vec<BackwardDirective>,
    /// Forward directives and detected source() calls
    pub sources: Vec<ForwardSource>,
    /// Statically recognized top-level `{targets}` `tar_source()` calls.
    ///
    /// Requests remain durable even when none of their paths currently exist.
    /// Filesystem expansion is a separate, workspace-aware phase performed
    /// after working-directory enrichment.
    #[serde(default)]
    pub tar_source_requests: Vec<TarSourceRequest>,
    /// Statically recognized bounded `list.files()` / `for` source batches.
    #[serde(default)]
    pub list_files_source_requests: Vec<ListFilesSourceRequest>,
    /// Existing and potential filesystem roots retained by the last detached
    /// source-batch expansion, including real targets of followed symlinks.
    ///
    /// The historical field name is retained for metadata compatibility.
    #[serde(default)]
    pub tar_source_expansion_watch_paths: Vec<std::path::PathBuf>,
    /// Implicit Shiny application context derived during filesystem enrichment.
    ///
    /// This is retained even for candidate files in an incomplete layout so the
    /// application directory remains watched for entry-file mode transitions.
    #[serde(default)]
    pub shiny_application: Option<ShinyApplicationMetadata>,
    /// Working directory override (explicit `# raven: cd`)
    pub working_directory: Option<String>,
    /// Shiny application directory used while convention-loaded files execute.
    #[serde(default)]
    pub application_working_directory: Option<String>,
    /// Working directory inherited from parent via backward directive.
    /// This is populated when a file has a backward directive (`# raven: sourced-by`, etc.)
    /// pointing to a parent file, and the parent has an effective working directory.
    /// Priority for path resolution: explicit working_directory > inherited > file's directory.
    pub inherited_working_directory: Option<String>,
    /// Lines with a line-scoped ignore (`# raven: ignore`, alias `@lsp-ignore`),
    /// 0-based, mapped to what each suppresses.
    pub ignored_lines: HashMap<u32, LineSuppression>,
    /// Lines targeted by a next-line ignore (`# raven: ignore-next`, alias
    /// `@lsp-ignore-next`), 0-based, mapped to what each suppresses.
    pub ignored_next_lines: HashMap<u32, LineSuppression>,
    /// File-level ignore (`# raven: ignore-file`), if present. Suppresses the
    /// matching analyzer diagnostics on every line in the file. Header-only.
    #[serde(default)]
    pub ignored_file: Option<LineSuppression>,
    /// Block/range ignores (`# raven: ignore-start` … `# raven: ignore-end`).
    /// Each entry is `(start_line, end_line_inclusive, what)`, 0-based.
    #[serde(default)]
    pub ignored_ranges: Vec<SuppressionRange>,
    /// All parsed suppression directives (both `ignore` and `expect` flavors),
    /// retained for the `unused-suppression` sweep (F2 Step 3). Separate from
    /// the inline `ignored_*` maps, which are keyed by *target* line for fast
    /// per-diagnostic lookup; this list keeps each directive's own line and
    /// flavor so an unused directive can be reported at its source.
    #[serde(default)]
    pub suppression_directives: Vec<SuppressionDirective>,
    /// Detected package-load calls, including static pacman `p_load()` forms.
    pub library_calls: Vec<LibraryCall>,
    /// File/pipeline-level worker packages declared by statically recognized
    /// top-level `tar_option_set(packages = ...)` calls.
    #[serde(default)]
    pub targets_pipeline_packages: Vec<TargetsPackageDeclaration>,
    /// Variables declared via `# raven: var` directives
    #[serde(default)]
    pub declared_variables: Vec<DeclaredSymbol>,
    /// Functions declared via `# raven: func` directives
    #[serde(default)]
    pub declared_functions: Vec<DeclaredSymbol>,
    /// NSE contracts declared via `# raven: nse` directives.
    #[serde(default)]
    pub nse_declarations: Vec<NseDeclaration>,
    /// Callee-side `# raven: standalone` directive (issue #479). When `true`,
    /// this file is a self-contained "module": **when computing its own
    /// diagnostics** its cross-file scope is resolved in ISOLATION from the files
    /// that `source()` it — its backward parent-prefix walk is skipped, so it
    /// inherits no symbols or packages from its callers (those are the only
    /// caller contributions the backward walk carries; `DataAliasProvider` and
    /// working directory are forward-threaded, not backward-inherited — see the
    /// shipped-scope note below). It still contributes its own definitions AND its own
    /// loaded packages forward to callers (the additive forward merge is
    /// unchanged). Header-only (must appear before any code). Opt-in and
    /// safe-direction: a mislabeled standalone file can at worst raise a false
    /// "undefined" INSIDE itself, never hide a real bug in a caller. See
    /// `docs/directives.md`.
    ///
    /// SHIPPED SCOPE: only this backward-walk skip ("part 1") shipped. The
    /// caller-independent forward-child resolution ("part 2" — dropping a
    /// caller's threaded packages/provider/cd when this file is resolved as that
    /// caller's forward child) is deferred to WI2b (#483); until then, a caller
    /// sourcing a standalone file still threads its own packages/provider/cd into
    /// the child's forward resolution.
    #[serde(default)]
    pub standalone: bool,
    /// Detected `pkg::member` namespace references (issue #503).
    #[serde(default)]
    pub namespace_references: Vec<NamespaceReference>,
}

impl CrossFileMetadata {
    /// Package names referenced by lexical package loaders or the targets
    /// pipeline worker-package declaration.
    ///
    /// This is a warming/inventory view only. Semantic scope keeps the two
    /// channels separate because ordinary loaders are position-sensitive while
    /// targets packages are pipeline-level.
    pub fn referenced_packages(&self) -> impl Iterator<Item = &str> {
        self.library_calls
            .iter()
            .map(|call| call.package.as_str())
            .chain(
                self.targets_pipeline_packages
                    .iter()
                    .map(|declaration| declaration.package.as_str()),
            )
    }
}

/// One statically recognized top-level `{targets}` `tar_source()` call.
///
/// `files` preserves the user's vector order. Expansion and first-occurrence
/// deduplication are scoped to this request: separate calls that name the same
/// script represent separate executions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub struct TarSourceRequest {
    /// Statically resolved `files` argument, or `["R"]` when omitted.
    #[serde(default)]
    pub files: Vec<String>,
    /// 0-based line of the call.
    #[serde(default)]
    pub line: u32,
    /// 0-based UTF-16 column of the call.
    #[serde(default)]
    pub column: u32,
    /// Static `change_directory = TRUE` value.
    #[serde(default)]
    pub change_directory: bool,
}

/// One bounded top-level `list.files()` / `for (...) source(...)` request.
///
/// Detection is filesystem-free. Expansion enumerates the immediate members
/// of `directory` after working-directory enrichment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub struct ListFilesSourceRequest {
    /// Literal directory passed to `list.files()`.
    pub directory: String,
    /// 0-based line of the `source(iterator)` call.
    pub line: u32,
    /// 0-based UTF-16 column of the `source(iterator)` call.
    pub column: u32,
}

/// Origin of an ordered source batch.
///
/// `tar_source_ordinal` remains the serialized ordinal carrier for backward
/// compatibility; this discriminator keeps tar-only contextual path behavior
/// from leaking into ordinary `list.files()` source loops.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceBatchKind {
    TarSource,
    ListFiles,
    /// Legacy `global.R`, evaluated in the global environment before support.
    ShinyGlobal,
    /// Top-level `R/*.[Rr]` helpers evaluated in one shared support environment.
    Shiny,
}

impl SourceBatchKind {
    /// Whether this batch executes before any source position in its parent.
    pub(crate) fn is_pre_entry(self) -> bool {
        matches!(self, Self::ShinyGlobal | Self::Shiny)
    }
}

/// Selected implicit Shiny application mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ShinyApplicationMode {
    Legacy,
    SingleFile,
}

/// One file's role in an implicit Shiny application topology.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ShinyFileRole {
    /// A conventional candidate that is not active in the selected layout.
    Candidate,
    LegacyGlobal,
    Helper {
        ordinal: u32,
    },
    AppEntry,
    UiEntry,
    ServerEntry,
}

impl ShinyFileRole {
    pub(crate) fn is_entry(&self) -> bool {
        matches!(self, Self::AppEntry | Self::UiEntry | Self::ServerEntry)
    }

    pub(crate) fn helper_ordinal(&self) -> Option<u32> {
        match self {
            Self::Helper { ordinal } => Some(*ordinal),
            _ => None,
        }
    }
}

/// Visibility of one Shiny participant's foreign declarations from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShinyDeclarationVisibility {
    Never,
    /// Visible only while analyzing a function body, after the shared support
    /// environment has been completely populated.
    DeferredOnly,
    Always,
}

/// Filesystem-derived implicit Shiny application context for one file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ShinyApplicationMetadata {
    /// Lexical application directory used as the runtime working directory.
    pub application_root: String,
    /// Canonical physical application identity used across URI aliases.
    ///
    /// Older serialized metadata falls back to `application_root`.
    #[serde(default)]
    pub application_identity: Option<String>,
    /// `None` for an incomplete candidate layout without `server.R` or `app.R`.
    pub mode: Option<ShinyApplicationMode>,
    pub role: ShinyFileRole,
}

impl ShinyApplicationMetadata {
    fn identity(&self) -> &str {
        self.application_identity
            .as_deref()
            .unwrap_or(&self.application_root)
    }

    /// Whether this file participates in the selected implicit runtime layout.
    ///
    /// Candidate metadata is retained to watch incomplete and mode-incompatible
    /// conventional paths, but candidates must keep ordinary file semantics.
    pub(crate) fn is_active_participant(&self) -> bool {
        self.mode.is_some() && !matches!(self.role, ShinyFileRole::Candidate)
    }

    pub(crate) fn is_same_active_application(&self, other: &Self) -> bool {
        self.is_active_participant()
            && other.is_active_participant()
            && self.mode == other.mode
            && self.identity() == other.identity()
    }

    /// Classify declaration visibility from `declaration` into `self`.
    ///
    /// Returns `None` when the files do not belong to the same active implicit
    /// application; callers then retain ordinary source-graph propagation.
    /// Within one application, legacy global declarations flow forward into
    /// support and entries, helper declarations flow eagerly only to later
    /// helpers but late-bind throughout completed support function bodies, and
    /// entry-local declarations never flow to support or sibling entries.
    pub(crate) fn declaration_visibility_from(
        &self,
        declaration: &Self,
    ) -> Option<ShinyDeclarationVisibility> {
        if !self.is_same_active_application(declaration) {
            return None;
        }

        use ShinyDeclarationVisibility::{Always, DeferredOnly, Never};
        let visibility = match (&self.role, &declaration.role) {
            // Entry-local and inactive-candidate declarations never lend to
            // another participant in the implicit application.
            (_, ShinyFileRole::Candidate)
            | (_, ShinyFileRole::AppEntry)
            | (_, ShinyFileRole::UiEntry)
            | (_, ShinyFileRole::ServerEntry) => Never,

            // Legacy global executes before support and entries, but does not
            // see declarations made by those later environments.
            (ShinyFileRole::Helper { .. }, ShinyFileRole::LegacyGlobal) => Always,
            (role, ShinyFileRole::LegacyGlobal) if role.is_entry() => Always,
            (_, ShinyFileRole::LegacyGlobal) => Never,

            // Entries are created after the support batch is complete.
            (role, ShinyFileRole::Helper { .. }) if role.is_entry() => Always,

            // Eager helper code sees only the ordered prefix. Function bodies
            // close over the completed shared support environment.
            (query, declaration) => match (query.helper_ordinal(), declaration.helper_ordinal()) {
                (Some(query), Some(declaration)) if declaration < query => Always,
                (Some(query), Some(declaration)) if declaration != query => DeferredOnly,
                _ => Never,
            },
        };
        Some(visibility)
    }
}

impl CrossFileMetadata {
    /// True if this file carries any cross-file NSE/func directive material —
    /// a non-empty `nse_declarations` OR a non-empty `declared_functions`.
    ///
    /// This is the per-file half of the short-circuit guarding
    /// `collect_cross_file_nse` (in `crate::handlers`): that collector reads ONLY
    /// these two fields (it walks the revalidation-consistent set and, for each
    /// member, consults `member.nse_declarations` and `member.declared_functions`).
    /// So if no metadata entry the collector could read returns `true` here, the
    /// collected result is necessarily `{ nse: [], funcs: [] }` — which is why
    /// `DiagnosticsSnapshot::build` ORs this predicate across the neighborhood
    /// `metadata_map` (the exact set the collector reads) into the
    /// `any_nse_or_func_directives` signal that drives the skip.
    pub fn has_nse_or_func_directives(&self) -> bool {
        !self.nse_declarations.is_empty() || !self.declared_functions.is_empty()
    }

    /// Whether this file owns filesystem-derived ordered source topology.
    pub fn has_source_batch_topology(&self) -> bool {
        !self.tar_source_requests.is_empty()
            || !self.list_files_source_requests.is_empty()
            || self.shiny_application.is_some()
    }
}

/// A backward directive declaring this file is sourced by another
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackwardDirective {
    pub path: String,
    pub call_site: CallSiteSpec,
    /// 0-based line where the directive appears
    pub directive_line: u32,
}

/// Where a statically detected source call evaluates its child.
///
/// This is deliberately more precise than the legacy serialized `local`
/// boolean: both [`CurrentFrame`](Self::CurrentFrame) and
/// [`NonInheriting`](Self::NonInheriting) are non-global, but only the current
/// frame can lend ordinary caller bindings to the child. Legacy booleans exist
/// only on the `ForwardSource` wire format; runtime code carries this enum alone.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum SourceLocality {
    /// The child is evaluated in the process global environment.
    #[default]
    Global,
    /// The child is evaluated in the frame currently evaluating `source()`.
    CurrentFrame,
    /// The destination is external or cannot be proven to inherit into Raven's
    /// lexical scope model.
    NonInheriting,
}

impl SourceLocality {
    /// Compose a source destination with the frame evaluating its call.
    ///
    /// `CurrentFrame` is relative: a global capture frame promotes it to
    /// `Global`, while an external or unknown capture frame makes it
    /// `NonInheriting`. Absolute global and already-non-inheriting destinations
    /// are unchanged.
    pub(crate) fn relative_to(self, frame: super::binding::CaptureEvaluationFrame) -> Self {
        use super::binding::CaptureEvaluationFrame;

        match (self, frame) {
            (Self::CurrentFrame, CaptureEvaluationFrame::Global) => Self::Global,
            (Self::CurrentFrame, CaptureEvaluationFrame::ExternalOrUnknown) => Self::NonInheriting,
            _ => self,
        }
    }
}

/// A forward source (directive or detected source() call).
///
/// [`SourceLocality`] is the sole runtime destination representation. Custom
/// serde implementations retain the legacy `local` and
/// `sys_source_global_env` fields only at the wire boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ForwardSource {
    pub path: String,
    /// 0-based line
    pub line: u32,
    /// 0-based UTF-16 column
    pub column: u32,
    /// true if `# raven: source` directive, false if detected source()
    pub is_directive: bool,
    /// Precise destination class for scope and package propagation.
    pub locality: SourceLocality,
    /// source(..., chdir = TRUE)
    pub chdir: bool,
    /// true for sys.source(), false for source()
    pub is_sys_source: bool,
    /// true if the directive had an explicit `line=N` parameter
    /// Used to determine if redundancy diagnostics should be emitted.
    /// Only relevant when is_directive=true.
    /// _Requirements: 6.2_
    pub explicit_line: bool,
    /// 0-based line where the directive itself appears in the file.
    /// Only relevant when is_directive=true.
    /// Used for diagnostic positioning when line= parameter is invalid.
    pub directive_line: u32,
    /// true if the user explicitly specified `line=0` (invalid value).
    /// Line numbers in directives are 1-based, so line=0 is invalid.
    /// When true, a warning diagnostic should be emitted.
    /// Only relevant when is_directive=true and explicit_line=true.
    pub user_line_zero: bool,
    /// true if the source() call is lexically inside a function body.
    ///
    /// Function-body source() calls only execute when the enclosing function
    /// is invoked, so they are not load-time ordering constraints for
    /// top-level usages. Used by the "used before it's available" diagnostic
    /// to skip blame attribution. Always false for `# raven: source` directives,
    /// which are header-only and run at load time.
    pub is_function_scoped: bool,
    /// If the `source()` file argument is a `system.file(...)` call with
    /// statically determinable string-literal parts and package, store the
    /// extracted call here. Resolution is deferred to the path-resolve layer
    /// because it needs workspace and library-path information unavailable at
    /// parse time. When `Some`, `path` is empty.
    pub system_file: Option<super::source_detect::SystemFileCall>,
    /// Pre-resolved absolute file URI for cross-package `system.file()` targets.
    /// When set, dependency and scope resolution use this directly instead of
    /// calling `resolve_path` (which can't handle true absolute paths outside
    /// the workspace). Set by `resolve_system_file_sources` for branch-2 hits.
    pub resolved_uri: Option<tower_lsp::lsp_types::Url>,
    /// Ordered child index within one expanded source batch.
    ///
    /// The historical name is retained for wire compatibility. The call
    /// identity is `(line, column, source_batch_kind)`, and the ordinal
    /// disambiguates members sharing it.
    pub tar_source_ordinal: Option<u32>,
    /// Explicit source-batch origin. Legacy metadata with an ordinal and no
    /// kind is interpreted as [`SourceBatchKind::TarSource`].
    pub source_batch_kind: Option<SourceBatchKind>,
    /// Whether this exact source call is the sole consequence of
    /// `if (file.exists("path"))` for the same literal path.
    ///
    /// The source remains an ordinary dependency when the file exists. This
    /// bit affects only path diagnostics: an absent or case-only-mismatched
    /// guarded path makes the branch inert rather than causing `source()` to
    /// fail. Outside-workspace diagnostics remain active for existing files.
    pub guarded_by_file_exists: bool,
}

fn default_sys_source_global_env() -> bool {
    true
}

#[derive(Deserialize)]
struct ForwardSourceWire {
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: u32,
    #[serde(default)]
    column: u32,
    #[serde(default)]
    is_directive: bool,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    locality: SourceLocality,
    #[serde(default)]
    chdir: bool,
    #[serde(default)]
    is_sys_source: bool,
    #[serde(default = "default_sys_source_global_env")]
    sys_source_global_env: bool,
    #[serde(default)]
    explicit_line: bool,
    #[serde(default)]
    directive_line: u32,
    #[serde(default)]
    user_line_zero: bool,
    #[serde(default)]
    is_function_scoped: bool,
    #[serde(default)]
    system_file: Option<super::source_detect::SystemFileCall>,
    #[serde(default)]
    resolved_uri: Option<tower_lsp::lsp_types::Url>,
    #[serde(default)]
    tar_source_ordinal: Option<u32>,
    #[serde(default)]
    source_batch_kind: Option<SourceBatchKind>,
    #[serde(default)]
    guarded_by_file_exists: bool,
}

impl Serialize for ForwardSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let local = !self.is_sys_source && self.locality != SourceLocality::Global;
        let sys_source_global_env = !self.is_sys_source || self.locality == SourceLocality::Global;
        let mut state = serializer.serialize_struct("ForwardSource", 18)?;
        state.serialize_field("path", &self.path)?;
        state.serialize_field("line", &self.line)?;
        state.serialize_field("column", &self.column)?;
        state.serialize_field("is_directive", &self.is_directive)?;
        state.serialize_field("local", &local)?;
        state.serialize_field("locality", &self.locality)?;
        state.serialize_field("chdir", &self.chdir)?;
        state.serialize_field("is_sys_source", &self.is_sys_source)?;
        state.serialize_field("sys_source_global_env", &sys_source_global_env)?;
        state.serialize_field("explicit_line", &self.explicit_line)?;
        state.serialize_field("directive_line", &self.directive_line)?;
        state.serialize_field("user_line_zero", &self.user_line_zero)?;
        state.serialize_field("is_function_scoped", &self.is_function_scoped)?;
        state.serialize_field("system_file", &self.system_file)?;
        state.serialize_field("resolved_uri", &self.resolved_uri)?;
        state.serialize_field("tar_source_ordinal", &self.tar_source_ordinal)?;
        state.serialize_field("source_batch_kind", &self.source_batch_kind)?;
        state.serialize_field("guarded_by_file_exists", &self.guarded_by_file_exists)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ForwardSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ForwardSourceWire::deserialize(deserializer)?;
        let locality = if wire.locality != SourceLocality::Global {
            wire.locality
        } else if wire.is_sys_source && !wire.sys_source_global_env {
            SourceLocality::NonInheriting
        } else if wire.local {
            SourceLocality::CurrentFrame
        } else {
            SourceLocality::Global
        };
        Ok(Self {
            path: wire.path,
            line: wire.line,
            column: wire.column,
            is_directive: wire.is_directive,
            locality,
            chdir: wire.chdir,
            is_sys_source: wire.is_sys_source,
            explicit_line: wire.explicit_line,
            directive_line: wire.directive_line,
            user_line_zero: wire.user_line_zero,
            is_function_scoped: wire.is_function_scoped,
            system_file: wire.system_file,
            resolved_uri: wire.resolved_uri,
            tar_source_ordinal: wire.tar_source_ordinal,
            source_batch_kind: wire.source_batch_kind,
            guarded_by_file_exists: wire.guarded_by_file_exists,
        })
    }
}

impl ForwardSource {
    /// Whether this source is one member of an ordered expansion batch.
    pub fn is_source_batch_member(&self) -> bool {
        self.tar_source_ordinal.is_some()
    }

    /// Whether this source uses `{targets}` contextual path semantics.
    pub fn is_tar_source_member(&self) -> bool {
        self.tar_source_ordinal.is_some()
            && self
                .source_batch_kind
                .is_none_or(|kind| kind == SourceBatchKind::TarSource)
    }

    /// True when missing-file/path diagnostics must skip this source.
    ///
    /// `system.file()` sources mostly carry no literal path to diagnose: a
    /// branch-2 resolved one (`resolved_uri` set) points outside the
    /// workspace, and an unresolved one (e.g. an uninstalled package, or
    /// branch-2 resolution deferred while lib_paths is empty) has an empty
    /// `path` and must degrade silently rather than emit a spurious
    /// "Cannot resolve path: ''". The exception is a branch-1 self-package
    /// hit, whose workspace-relative `/inst/...` path IS diagnosable —
    /// `system_file` stays `Some` on every system.file entry for
    /// re-resolution (see `resolve_system_file_sources`), so "unresolved"
    /// is encoded as `system_file` Some + empty `path`, not by `system_file`
    /// presence alone.
    pub fn exempt_from_missing_file_diagnostics(&self) -> bool {
        self.resolved_uri.is_some() || (self.system_file.is_some() && self.path.is_empty())
    }

    /// Check if symbols from this source lend outward as global bindings.
    pub fn inherits_symbols(&self) -> bool {
        self.locality == SourceLocality::Global
    }
}

/// Call site specification for backward directives
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum CallSiteSpec {
    /// Use configuration default
    #[default]
    Default,
    /// Explicit line number (0-based internally, converted from 1-based user input)
    Line(u32),
    /// Pattern to match in parent file
    Match(String),
}

/// Convert a byte offset to UTF-16 column for a given line.
///
/// Re-export of [`crate::utf16::byte_offset_to_utf16_column`] for backward
/// compatibility with existing imports under `crate::cross_file::types`.
/// Keep one implementation so the two callers cannot drift on edge cases
/// (non-boundary byte offsets, surrogate pairs, etc.).
pub use crate::utf16::byte_offset_to_utf16_column;

/// Enrich metadata with inherited working directory from parent files.
///
/// Only sets `inherited_working_directory` when:
/// - `sourced_by` is not empty (file has backward directives)
/// - `working_directory` is None (no explicit `# raven: cd`)
///
/// Uses `compute_inherited_working_directory` from dependency module.
pub fn enrich_metadata_with_inherited_wd<F>(
    meta: &mut CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
    get_metadata: F,
    max_depth: usize,
) where
    F: Fn(&Url) -> Option<std::sync::Arc<CrossFileMetadata>>,
{
    if meta.sourced_by.is_empty() || meta.working_directory.is_some() {
        return;
    }
    meta.inherited_working_directory =
        super::dependency::compute_inherited_working_directory_with_depth(
            uri,
            meta,
            workspace_root,
            get_metadata,
            max_depth,
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shiny(role: ShinyFileRole, mode: ShinyApplicationMode) -> ShinyApplicationMetadata {
        ShinyApplicationMetadata {
            application_root: "/workspace/app".to_string(),
            application_identity: None,
            mode: Some(mode),
            role,
        }
    }

    #[test]
    fn shiny_declaration_visibility_matches_environment_phases() {
        use ShinyDeclarationVisibility::{Always, DeferredOnly, Never};
        use ShinyFileRole::{AppEntry, Candidate, Helper, LegacyGlobal, ServerEntry, UiEntry};

        let global = shiny(LegacyGlobal, ShinyApplicationMode::Legacy);
        let first = shiny(Helper { ordinal: 0 }, ShinyApplicationMode::Legacy);
        let second = shiny(Helper { ordinal: 1 }, ShinyApplicationMode::Legacy);
        let ui = shiny(UiEntry, ShinyApplicationMode::Legacy);
        let server = shiny(ServerEntry, ShinyApplicationMode::Legacy);

        assert_eq!(first.declaration_visibility_from(&global), Some(Always));
        assert_eq!(global.declaration_visibility_from(&first), Some(Never));
        assert_eq!(second.declaration_visibility_from(&first), Some(Always));
        assert_eq!(
            first.declaration_visibility_from(&second),
            Some(DeferredOnly)
        );
        assert_eq!(ui.declaration_visibility_from(&second), Some(Always));
        assert_eq!(server.declaration_visibility_from(&ui), Some(Never));

        let app = shiny(AppEntry, ShinyApplicationMode::SingleFile);
        let single_helper = shiny(Helper { ordinal: 0 }, ShinyApplicationMode::SingleFile);
        assert_eq!(
            app.declaration_visibility_from(&single_helper),
            Some(Always)
        );
        assert_eq!(single_helper.declaration_visibility_from(&app), Some(Never));

        let candidate = shiny(Candidate, ShinyApplicationMode::Legacy);
        assert!(!candidate.is_active_participant());
        assert!(!candidate.is_same_active_application(&server));
        assert!(!server.is_same_active_application(&candidate));
        assert_eq!(candidate.declaration_visibility_from(&server), None);
        assert_eq!(server.declaration_visibility_from(&candidate), None);

        let mut other = second.clone();
        other.application_root = "/workspace/other".to_string();
        assert_eq!(first.declaration_visibility_from(&other), None);

        let mut alias = first.clone();
        alias.application_root = "/workspace/alias".to_string();
        alias.application_identity = Some("/workspace/real".to_string());
        let mut canonical = second.clone();
        canonical.application_identity = Some("/workspace/real".to_string());
        assert_eq!(
            alias.declaration_visibility_from(&canonical),
            Some(DeferredOnly)
        );
    }

    /// Four-state matrix for the missing-file-diagnostics exemption: only a
    /// branch-2 resolved entry (resolved_uri set) or an inert unresolved
    /// system.file entry (empty path) is exempt; a branch-1 "/inst/..." hit
    /// and an ordinary path source remain diagnosable.
    #[test]
    fn exempt_from_missing_file_diagnostics_matrix() {
        let sf = || {
            Some(crate::cross_file::source_detect::SystemFileCall {
                parts: vec!["helper.R".to_string()],
                package: "pkg".to_string(),
            })
        };

        // Branch-2 resolved: points outside the workspace → exempt.
        let resolved = ForwardSource {
            system_file: sf(),
            path: "/lib/pkg/helper.R".to_string(),
            resolved_uri: Some(
                tower_lsp::lsp_types::Url::parse("file:///lib/pkg/helper.R").unwrap(),
            ),
            ..Default::default()
        };
        assert!(resolved.exempt_from_missing_file_diagnostics());

        // Unresolved/deferred: empty path, inert → exempt.
        let unresolved = ForwardSource {
            system_file: sf(),
            ..Default::default()
        };
        assert!(unresolved.exempt_from_missing_file_diagnostics());

        // Branch-1 self-package hit: workspace-relative path IS diagnosable.
        let branch1 = ForwardSource {
            system_file: sf(),
            path: "/inst/helper.R".to_string(),
            ..Default::default()
        };
        assert!(!branch1.exempt_from_missing_file_diagnostics());

        // Ordinary path source: diagnosable.
        let plain = ForwardSource {
            path: "helper.R".to_string(),
            ..Default::default()
        };
        assert!(!plain.exempt_from_missing_file_diagnostics());
    }

    #[test]
    fn test_byte_offset_to_utf16_column_ascii() {
        let line = "hello world";
        assert_eq!(byte_offset_to_utf16_column(line, 0), 0);
        assert_eq!(byte_offset_to_utf16_column(line, 5), 5);
        assert_eq!(byte_offset_to_utf16_column(line, 11), 11);
    }

    #[test]
    fn test_byte_offset_to_utf16_column_emoji() {
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let line = "a🎉b";
        assert_eq!(byte_offset_to_utf16_column(line, 0), 0); // before 'a'
        assert_eq!(byte_offset_to_utf16_column(line, 1), 1); // after 'a', before emoji
        assert_eq!(byte_offset_to_utf16_column(line, 5), 3); // after emoji (1 + 2 UTF-16 units)
        assert_eq!(byte_offset_to_utf16_column(line, 6), 4); // after 'b'
    }

    #[test]
    fn test_byte_offset_to_utf16_column_cjk() {
        // CJK characters are 3 bytes in UTF-8, 1 UTF-16 code unit each
        let line = "a中b";
        assert_eq!(byte_offset_to_utf16_column(line, 0), 0); // before 'a'
        assert_eq!(byte_offset_to_utf16_column(line, 1), 1); // after 'a'
        assert_eq!(byte_offset_to_utf16_column(line, 4), 2); // after '中'
        assert_eq!(byte_offset_to_utf16_column(line, 5), 3); // after 'b'
    }

    #[test]
    fn test_call_site_spec_default() {
        assert_eq!(CallSiteSpec::default(), CallSiteSpec::Default);
    }

    #[test]
    fn source_locality_composes_with_capture_frame() {
        use crate::cross_file::binding::CaptureEvaluationFrame;

        for (locality, frame, expected) in [
            (
                SourceLocality::CurrentFrame,
                CaptureEvaluationFrame::Caller,
                SourceLocality::CurrentFrame,
            ),
            (
                SourceLocality::CurrentFrame,
                CaptureEvaluationFrame::Global,
                SourceLocality::Global,
            ),
            (
                SourceLocality::CurrentFrame,
                CaptureEvaluationFrame::ExternalOrUnknown,
                SourceLocality::NonInheriting,
            ),
            (
                SourceLocality::Global,
                CaptureEvaluationFrame::ExternalOrUnknown,
                SourceLocality::Global,
            ),
            (
                SourceLocality::NonInheriting,
                CaptureEvaluationFrame::Global,
                SourceLocality::NonInheriting,
            ),
        ] {
            assert_eq!(locality.relative_to(frame), expected);
        }
    }

    #[test]
    fn test_cross_file_metadata_serialization() {
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../main.R".to_string(),
                call_site: CallSiteSpec::Line(15),
                directive_line: 0,
            }],
            sources: vec![ForwardSource {
                path: "utils.R".to_string(),
                line: 5,
                column: 0,
                is_directive: false,
                locality: SourceLocality::NonInheriting,
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            working_directory: Some("/data".to_string()),
            inherited_working_directory: None,
            ignored_lines: HashMap::from([(10, LineSuppression::All), (20, LineSuppression::All)]),
            ignored_next_lines: HashMap::from([(15, LineSuppression::All)]),
            library_calls: vec![],
            declared_variables: vec![],
            declared_functions: vec![],
            ..Default::default()
        };

        // Round-trip serialization
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: CrossFileMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.sourced_by.len(), 1);
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.sources[0].locality, SourceLocality::NonInheriting);
        assert!(json.contains("NonInheriting"));
        assert_eq!(parsed.working_directory, Some("/data".to_string()));
        assert!(parsed.ignored_lines.contains_key(&10));
        assert!(parsed.ignored_next_lines.contains_key(&15));
    }

    #[test]
    fn forward_source_deserialization_normalizes_legacy_locality_fields() {
        for (json, expected) in [
            (r#"{"path":"child.R"}"#, SourceLocality::Global),
            (
                r#"{"path":"child.R","local":false}"#,
                SourceLocality::Global,
            ),
            (
                r#"{"path":"child.R","local":true}"#,
                SourceLocality::CurrentFrame,
            ),
            (
                r#"{"path":"child.R","is_sys_source":true,"sys_source_global_env":true}"#,
                SourceLocality::Global,
            ),
            (
                r#"{"path":"child.R","is_sys_source":true,"sys_source_global_env":false}"#,
                SourceLocality::NonInheriting,
            ),
            (
                r#"{"path":"child.R","locality":"CurrentFrame","local":false}"#,
                SourceLocality::CurrentFrame,
            ),
            (
                r#"{"path":"child.R","locality":"NonInheriting","local":false}"#,
                SourceLocality::NonInheriting,
            ),
            (
                r#"{"path":"child.R","locality":"Global","local":true}"#,
                SourceLocality::CurrentFrame,
            ),
        ] {
            let source: ForwardSource = serde_json::from_str(json).unwrap();
            assert_eq!(source.locality, expected, "{json}");
        }
    }

    #[test]
    fn forward_source_serialization_projects_legacy_locality_fields() {
        for (is_sys_source, locality, local, sys_global) in [
            (false, SourceLocality::Global, false, true),
            (false, SourceLocality::CurrentFrame, true, true),
            (false, SourceLocality::NonInheriting, true, true),
            (true, SourceLocality::Global, false, true),
            (true, SourceLocality::NonInheriting, false, false),
        ] {
            let source = ForwardSource {
                path: "child.R".to_string(),
                locality,
                is_sys_source,
                ..Default::default()
            };
            let value = serde_json::to_value(&source).unwrap();
            assert_eq!(value["locality"], serde_json::json!(locality));
            assert_eq!(value["local"], local);
            assert_eq!(value["sys_source_global_env"], sys_global);
            let round_trip: ForwardSource = serde_json::from_value(value).unwrap();
            assert_eq!(round_trip, source);
        }
    }

    #[test]
    fn forward_source_optional_guard_round_trips_and_defaults_false() {
        let legacy: ForwardSource = serde_json::from_str(r#"{"path":"child.R"}"#).unwrap();
        assert!(!legacy.guarded_by_file_exists);

        let guarded = ForwardSource {
            path: "child.R".to_string(),
            guarded_by_file_exists: true,
            ..Default::default()
        };
        let value = serde_json::to_value(&guarded).unwrap();
        assert_eq!(value["guarded_by_file_exists"], true);
        assert_eq!(
            serde_json::from_value::<ForwardSource>(value).unwrap(),
            guarded
        );
    }

    #[test]
    fn source_batch_kind_round_trips_and_legacy_ordinals_mean_tar() {
        let legacy: ForwardSource =
            serde_json::from_str(r#"{"path":"child.R","tar_source_ordinal":0}"#).unwrap();
        assert!(legacy.is_source_batch_member());
        assert!(legacy.is_tar_source_member());

        let list_files = ForwardSource {
            path: "child.R".to_string(),
            tar_source_ordinal: Some(0),
            source_batch_kind: Some(SourceBatchKind::ListFiles),
            ..Default::default()
        };
        assert!(list_files.is_source_batch_member());
        assert!(!list_files.is_tar_source_member());
        let value = serde_json::to_value(&list_files).unwrap();
        assert_eq!(value["source_batch_kind"], "ListFiles");
        assert_eq!(
            serde_json::from_value::<ForwardSource>(value).unwrap(),
            list_files
        );
    }

    #[test]
    fn test_cross_file_metadata_default_inherited_working_directory_is_none() {
        // Validates: Requirements 6.1
        // The default value for inherited_working_directory should be None
        let meta = CrossFileMetadata::default();
        assert!(meta.inherited_working_directory.is_none());
    }

    #[test]
    fn test_cross_file_metadata_serialization_with_inherited_working_directory() {
        // Validates: Requirements 6.1
        // Test serialization round-trip when inherited_working_directory has a value
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            sources: vec![],
            working_directory: None,
            inherited_working_directory: Some("/project/data".to_string()),
            ignored_lines: HashMap::new(),
            ignored_next_lines: HashMap::new(),
            library_calls: vec![],
            declared_variables: vec![],
            declared_functions: vec![],
            ..Default::default()
        };

        // Round-trip serialization
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: CrossFileMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed.inherited_working_directory,
            Some("/project/data".to_string())
        );
        assert!(parsed.working_directory.is_none());
    }

    #[test]
    fn test_cross_file_metadata_serialization_both_working_directories() {
        // Validates: Requirements 6.1
        // Test serialization when both explicit and inherited working directories are set
        // (This scenario represents a child file with its own @lsp-cd that also has a backward directive)
        let meta = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Match("source".to_string()),
                directive_line: 1,
            }],
            sources: vec![ForwardSource {
                path: "helper.R".to_string(),
                line: 10,
                column: 0,
                is_directive: false,
                chdir: false,
                is_sys_source: false,
                ..Default::default()
            }],
            working_directory: Some("/child/explicit".to_string()),
            inherited_working_directory: Some("/parent/inherited".to_string()),
            ignored_lines: HashMap::new(),
            ignored_next_lines: HashMap::new(),
            library_calls: vec![],
            declared_variables: vec![],
            declared_functions: vec![],
            ..Default::default()
        };

        // Round-trip serialization
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: CrossFileMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed.working_directory,
            Some("/child/explicit".to_string())
        );
        assert_eq!(
            parsed.inherited_working_directory,
            Some("/parent/inherited".to_string())
        );
        assert_eq!(parsed.sourced_by.len(), 1);
        assert_eq!(parsed.sources.len(), 1);
    }

    #[test]
    fn test_cross_file_metadata_json_field_presence() {
        // Validates: Requirements 6.1
        // Verify the JSON includes the inherited_working_directory field
        let meta = CrossFileMetadata {
            inherited_working_directory: Some("/test/path".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&meta).unwrap();

        // Verify the field name appears in the JSON
        assert!(json.contains("inherited_working_directory"));
        assert!(json.contains("/test/path"));
    }

    #[test]
    fn test_inherits_symbols_local_true() {
        let source = ForwardSource {
            path: "test.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            locality: crate::cross_file::types::SourceLocality::CurrentFrame,
            chdir: false,
            is_sys_source: false,
            ..Default::default()
        };
        assert!(!source.inherits_symbols());
    }

    #[test]
    fn test_inherits_symbols_sys_source_non_global() {
        let source = ForwardSource {
            path: "test.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            chdir: false,
            is_sys_source: true,
            locality: crate::cross_file::types::SourceLocality::NonInheriting,
            ..Default::default()
        };
        assert!(!source.inherits_symbols());
    }

    #[test]
    fn test_inherits_symbols_sys_source_global() {
        let source = ForwardSource {
            path: "test.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            chdir: false,
            is_sys_source: true,
            ..Default::default()
        };
        assert!(source.inherits_symbols());
    }

    #[test]
    fn test_inherits_symbols_regular_source() {
        let source = ForwardSource {
            path: "test.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            chdir: false,
            is_sys_source: false,
            ..Default::default()
        };
        assert!(source.inherits_symbols());
    }
}
