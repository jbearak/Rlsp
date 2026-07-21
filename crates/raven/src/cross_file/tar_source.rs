//! Filesystem-backed expansion of ordered static source-batch requests.
//!
//! Detection is filesystem-free and stores ordered [`TarSourceRequest`] and
//! bounded `list.files()` loop requests in metadata. This module classifies
//! those requests after working-directory enrichment. Tar requests recursively
//! enumerate `.R`/`.r`; list-files requests enumerate immediate `.R` members.
//!
//! Expansion has no process-global cache or lifecycle state. Callers own both
//! the prior authoritative metadata and any event-generation fencing needed
//! around the filesystem walk. [`expand_tar_source_requests`] returns a detached
//! value; [`apply_tar_source_expansion`] installs it into caller-owned metadata.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

use crate::config_file::exclusions::CompiledWorkspaceExclusions;

use super::path_resolve::{
    CaseMismatchRegime, PathContext, forward_child_path_context, forward_path_candidate_tiers,
    normalize_path_public, path_to_uri, resolve_path_with_workspace_fallback,
    resolve_source_path_rich,
};
use super::types::{
    CrossFileMetadata, ForwardSource, ListFilesSourceRequest, ShinyApplicationMetadata,
    SourceBatchKind, SourceLocality, TarSourceRequest,
};

const MAX_LIST_FILES_SOURCE_MEMBERS: usize = 256;

/// Detached result of expanding all requests in one metadata record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TarSourceExpansion {
    /// Ordered filesystem-derived forward sources.
    pub sources: Vec<ForwardSource>,
    /// Existing and potential membership roots, including symlink targets.
    pub watch_paths: Vec<PathBuf>,
    /// Filesystem-derived Shiny context for the enriched file.
    pub shiny_application: Option<ShinyApplicationMetadata>,
    /// Lexical Shiny application directory used for runtime path resolution.
    pub application_working_directory: Option<PathBuf>,
    /// Exact selected Shiny entry used to bootstrap direct support-file opens.
    pub selected_shiny_entry: Option<PathBuf>,
}

/// Context-sensitive provider files needed in addition to a URI-global graph
/// neighborhood when resolving finalized tar batches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextualTarProviders {
    /// Every provider reached from a tar member execution, sorted by URI.
    /// Callers subtract their already-collected graph neighborhood.
    pub providers: Vec<Url>,
    /// Distinct execution contexts in bounded traversal order. CLI callers use
    /// the first context for a provider that is not yet materialized.
    pub executions: Vec<ContextualProviderExecution>,
    /// Whether one URI was reached under more than one execution context or
    /// traversal was truncated before context equality could be proved.
    pub divergence: bool,
    /// Whether the shared visited/depth/per-URI-context budget stopped work.
    pub truncated: bool,
}

/// One context-sensitive provider execution discovered by the pure traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextualProviderExecution {
    pub uri: Url,
    pub context: PathContext,
    pub prefer_supplied_path_context: bool,
    /// False while walking the ordinary URI-global prefix before the first
    /// tar-derived edge. CLI materialization consumes both modes; only
    /// contextual executions are added to snapshot provider sets.
    pub contextual: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContextualProviderVisit {
    uri: Url,
    context: PathContext,
    mode: ContextualProviderMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ContextualProviderMode {
    GraphPrefix,
    Contextual { prefer_supplied_path_context: bool },
}

/// Result of looking up one ordinary source occurrence in the URI-global graph.
///
/// `Unknown` means the caller's graph view does not contain the parent URI,
/// while `Known(None)` means the parent is present but that occurrence has no
/// resolved edge. The distinction lets bounded snapshot callers fail closed on
/// a truncated graph without inventing a lexical target that scope would skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphPrefixEdgeLookup {
    Unknown,
    Known(Option<Url>),
}

/// Maximum distinct execution contexts retained for one provider URI.
///
/// The global visited cap remains the primary budget; this smaller local bound
/// prevents one repeated helper from consuming all of it.
const MAX_CONTEXTS_PER_PROVIDER_URI: usize = 16;

/// Collect the bounded ordinary-prefix and context-sensitive forward closure
/// rooted at a diagnostic target.
///
/// This is a pure traversal over detached metadata and a read-only graph-edge
/// lookup. Before the first tar-derived edge, ordinary sources match their
/// URI-global graph edge by call site and ordinal in `GraphPrefix` mode so a
/// downstream driver can host the batch. There is deliberately no lexical
/// fallback in that mode: scope skips metadata occurrences without a matching
/// graph edge, and the planner must follow the same prefix. Traversal is
/// deliberately locality-agnostic: scope executes even `NonInheriting` edges
/// (filtering symbols but merging process-wide package attachments), so the
/// planner follows every exact edge and leaves locality-specific merge
/// semantics entirely to scope — pruning here would starve a downstream tar
/// batch of its supplemental provider artifacts. A tar edge switches
/// permanently to contextual traversal, keyed only by provider URI, derived
/// [`PathContext`], and the two-valued lexical-first mode. Standalone children
/// re-anchor and sever that preference. `system.file()` targets use their
/// pre-resolved URI.
///
/// Prefix and contextual visits share one depth/visited/context budget. Any
/// truncation fails closed by setting `divergence`, which disables the
/// URI-keyed standalone cache. The collector performs no filesystem or graph
/// mutation. Because prefix selection reads graph edges, caller-side
/// memoization must include the graph edge revision.
pub fn collect_contextual_tar_providers<G, E>(
    root_uri: &Url,
    root_metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
    max_depth: usize,
    max_visited: usize,
    get_metadata: &G,
    get_graph_prefix_edge: &E,
) -> ContextualTarProviders
where
    G: Fn(&Url) -> Option<std::sync::Arc<CrossFileMetadata>>,
    E: Fn(&Url, &ForwardSource) -> GraphPrefixEdgeLookup,
{
    let Some(root_context) = PathContext::from_metadata(root_uri, root_metadata, workspace_root)
    else {
        return ContextualTarProviders::default();
    };
    let mut queue = VecDeque::from([(
        ContextualProviderVisit {
            uri: root_uri.clone(),
            context: root_context,
            mode: ContextualProviderMode::GraphPrefix,
        },
        0usize,
    )]);

    let mut result = ContextualTarProviders::default();
    let mut providers = HashSet::new();
    let mut visited = HashSet::new();
    let mut contexts_by_uri: HashMap<Url, HashSet<(PathContext, ContextualProviderMode)>> =
        HashMap::new();

    while let Some((visit, depth)) = queue.pop_front() {
        if visited.len() >= max_visited {
            result.truncated = true;
            result.divergence = true;
            break;
        }
        let contexts = contexts_by_uri.entry(visit.uri.clone()).or_default();
        let context_key = (visit.context.clone(), visit.mode);
        if !contexts.contains(&context_key) {
            if !contexts.is_empty() {
                result.divergence = true;
            }
            if contexts.len() >= MAX_CONTEXTS_PER_PROVIDER_URI {
                result.truncated = true;
                result.divergence = true;
                continue;
            }
            contexts.insert(context_key);
        }
        if !visited.insert(visit.clone()) {
            continue;
        }
        let contextual_preference = match visit.mode {
            ContextualProviderMode::GraphPrefix => None,
            ContextualProviderMode::Contextual {
                prefer_supplied_path_context,
            } => Some(prefer_supplied_path_context),
        };
        if contextual_preference.is_some() {
            providers.insert(visit.uri.clone());
        }
        if visit.uri != *root_uri {
            result.executions.push(ContextualProviderExecution {
                uri: visit.uri.clone(),
                context: visit.context.clone(),
                prefer_supplied_path_context: contextual_preference.unwrap_or(false),
                contextual: contextual_preference.is_some(),
            });
        }

        let metadata = if visit.uri == *root_uri {
            std::sync::Arc::new(root_metadata.clone())
        } else if let Some(metadata) = get_metadata(&visit.uri) {
            metadata
        } else {
            // The provider execution exists but its forward closure is unknown.
            // CLI callers may materialize it and repeat the traversal; LSP
            // callers must disable the URI-keyed standalone cache meanwhile.
            result.divergence = true;
            continue;
        };
        if matches!(visit.mode, ContextualProviderMode::GraphPrefix)
            && PathContext::from_metadata(&visit.uri, &metadata, workspace_root)
                .is_some_and(|global| global != visit.context)
            && metadata
                .sources
                .iter()
                .any(ForwardSource::is_tar_source_member)
        {
            result.divergence = true;
        }
        if depth >= max_depth {
            if !metadata.sources.is_empty() {
                result.truncated = true;
                result.divergence = true;
            }
            continue;
        }
        for source in &metadata.sources {
            let child_uri = match visit.mode {
                ContextualProviderMode::GraphPrefix => {
                    if source.is_tar_source_member() {
                        contextual_source_uri(source, &visit.context, true)
                    } else if let Some(uri) = source.resolved_uri.clone() {
                        Some(uri)
                    } else {
                        match get_graph_prefix_edge(&visit.uri, source) {
                            GraphPrefixEdgeLookup::Unknown => {
                                result.truncated = true;
                                result.divergence = true;
                                None
                            }
                            GraphPrefixEdgeLookup::Known(uri) => uri,
                        }
                    }
                }
                ContextualProviderMode::Contextual {
                    prefer_supplied_path_context,
                } => contextual_source_uri(source, &visit.context, prefer_supplied_path_context),
            };
            let Some(child_uri) = child_uri else {
                continue;
            };
            let child_is_standalone =
                get_metadata(&child_uri).is_some_and(|child| child.standalone);
            let Some(child_context) = forward_child_path_context(
                &child_uri,
                child_is_standalone,
                source.chdir,
                Some(&visit.context),
                workspace_root,
                get_metadata,
            ) else {
                continue;
            };
            let child_mode = match visit.mode {
                ContextualProviderMode::GraphPrefix if !source.is_tar_source_member() => {
                    ContextualProviderMode::GraphPrefix
                }
                ContextualProviderMode::GraphPrefix => ContextualProviderMode::Contextual {
                    prefer_supplied_path_context: !child_is_standalone,
                },
                ContextualProviderMode::Contextual {
                    prefer_supplied_path_context,
                } => ContextualProviderMode::Contextual {
                    prefer_supplied_path_context: (source.is_tar_source_member()
                        || prefer_supplied_path_context)
                        && !child_is_standalone,
                },
            };
            queue.push_back((
                ContextualProviderVisit {
                    uri: child_uri,
                    context: child_context,
                    mode: child_mode,
                },
                depth + 1,
            ));
        }
    }

    result.providers = providers.into_iter().collect();
    result
        .providers
        .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    result
}

fn contextual_source_uri(
    source: &ForwardSource,
    context: &PathContext,
    prefer_supplied_path_context: bool,
) -> Option<Url> {
    if source.system_file.is_some() {
        return source.resolved_uri.clone();
    }
    if source.is_tar_source_member() {
        return source.resolved_uri.clone().or_else(|| {
            resolve_path_with_workspace_fallback(&source.path, context)
                .and_then(|path| path_to_uri(&path))
        });
    }
    let lexical = || {
        resolve_path_with_workspace_fallback(&source.path, context)
            .and_then(|path| path_to_uri(&path))
    };
    if prefer_supplied_path_context {
        lexical().or_else(|| source.resolved_uri.clone())
    } else {
        source.resolved_uri.clone().or_else(lexical)
    }
}

#[derive(Debug, Clone, Default)]
struct RequestExpansion {
    files: Vec<PathBuf>,
    watch_paths: Vec<PathBuf>,
}

/// Expand every static `tar_source()` request without mutating metadata.
///
/// Each request has its own first-occurrence deduplication set and ordinal
/// sequence; separate calls that reach the same URI remain distinct. The
/// caller must invoke this after inherited-working-directory enrichment and
/// without holding shared state locks.
pub fn expand_tar_source_requests(
    metadata: &CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
) -> TarSourceExpansion {
    expand_tar_source_requests_with_exclusions(
        metadata,
        uri,
        workspace_root,
        &CompiledWorkspaceExclusions::default(),
    )
}

/// Expand every static source-batch request while honoring project exclusions.
pub fn expand_tar_source_requests_with_exclusions(
    metadata: &CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
    exclusions: &CompiledWorkspaceExclusions,
) -> TarSourceExpansion {
    let shiny = super::shiny::discover_shiny_application(uri, exclusions);
    let mut effective_metadata = metadata.clone();
    effective_metadata.application_working_directory = shiny
        .application_working_directory
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    effective_metadata.shiny_application = shiny.metadata.clone();
    let Some(context) = PathContext::from_metadata(uri, &effective_metadata, workspace_root) else {
        return TarSourceExpansion::default();
    };
    let mut result = TarSourceExpansion {
        watch_paths: shiny.watch_paths,
        shiny_application: shiny.metadata,
        application_working_directory: shiny.application_working_directory,
        selected_shiny_entry: shiny.selected_entry,
        ..TarSourceExpansion::default()
    };
    for request in &metadata.tar_source_requests {
        let expansion = expand_request(request, &context);
        result.watch_paths.extend(expansion.watch_paths);
        for (ordinal, path) in expansion.files.into_iter().enumerate() {
            let Some(resolved_uri) = path_to_uri(&path) else {
                continue;
            };
            result.sources.push(ForwardSource {
                path: path.to_string_lossy().into_owned(),
                line: request.line,
                column: request.column,
                locality: SourceLocality::Global,
                chdir: request.change_directory,
                // `targets::tar_source()` evaluates scripts in its selected
                // environment; accepted requests are restricted to the
                // default/global destination.
                is_sys_source: false,
                resolved_uri: Some(resolved_uri),
                tar_source_ordinal: Some(ordinal as u32),
                source_batch_kind: Some(SourceBatchKind::TarSource),
                ..Default::default()
            });
        }
    }
    for request in &metadata.list_files_source_requests {
        let expansion = expand_list_files_request(request, &context, workspace_root);
        result.watch_paths.extend(expansion.watch_paths);
        for (ordinal, path) in expansion.files.into_iter().enumerate() {
            let Some(resolved_uri) = path_to_uri(&path) else {
                continue;
            };
            result.sources.push(ForwardSource {
                path: path.to_string_lossy().into_owned(),
                line: request.line,
                column: request.column,
                locality: SourceLocality::Global,
                resolved_uri: Some(resolved_uri),
                tar_source_ordinal: Some(ordinal as u32),
                source_batch_kind: Some(SourceBatchKind::ListFiles),
                ..Default::default()
            });
        }
    }
    if let Some(path) = shiny.global
        && let Some(resolved_uri) = path_to_uri(&path)
    {
        result.sources.push(ForwardSource {
            path: path.to_string_lossy().into_owned(),
            locality: SourceLocality::Global,
            resolved_uri: Some(resolved_uri),
            tar_source_ordinal: Some(0),
            source_batch_kind: Some(SourceBatchKind::ShinyGlobal),
            ..Default::default()
        });
    }
    for (ordinal, path) in shiny.helpers.into_iter().enumerate() {
        if let Some(resolved_uri) = path_to_uri(&path) {
            result.sources.push(ForwardSource {
                path: path.to_string_lossy().into_owned(),
                locality: SourceLocality::Global,
                resolved_uri: Some(resolved_uri),
                tar_source_ordinal: Some(ordinal as u32),
                source_batch_kind: Some(SourceBatchKind::Shiny),
                ..Default::default()
            });
        }
    }
    result.watch_paths.sort();
    result.watch_paths.dedup();
    result.sources.sort_by_key(|source| {
        (
            source.line,
            source.column,
            source.source_batch_kind,
            source.tar_source_ordinal,
        )
    });
    result
}

/// Replace the derived tar portion of `metadata` with `expansion`.
///
/// Syntax-derived requests and ordinary/directive sources are preserved.
pub fn apply_tar_source_expansion(metadata: &mut CrossFileMetadata, expansion: TarSourceExpansion) {
    metadata
        .sources
        .retain(|source| !source.is_source_batch_member());
    metadata.sources.extend(expansion.sources);
    metadata.tar_source_expansion_watch_paths = expansion.watch_paths;
    metadata.application_working_directory = expansion
        .application_working_directory
        .map(|path| path.to_string_lossy().into_owned());
    metadata.shiny_application = expansion.shiny_application;
    metadata.sources.sort_by_key(|source| {
        (
            source.line,
            source.column,
            source.source_batch_kind,
            source.tar_source_ordinal,
        )
    });
}

/// Expand and install all static requests into caller-owned metadata.
pub fn finalize_tar_source_requests(
    metadata: &mut CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
) {
    let expansion = expand_tar_source_requests(metadata, uri, workspace_root);
    apply_tar_source_expansion(metadata, expansion);
}

/// Expand and install source batches while honoring project exclusions.
///
/// Returns the exact selected Shiny host when the file belongs to an active
/// application, allowing open-install prerequisite convergence to materialize
/// the host before diagnostics.
pub fn finalize_tar_source_requests_with_exclusions(
    metadata: &mut CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
    exclusions: &CompiledWorkspaceExclusions,
) -> Option<PathBuf> {
    let expansion =
        expand_tar_source_requests_with_exclusions(metadata, uri, workspace_root, exclusions);
    let selected_shiny_entry = expansion.selected_shiny_entry.clone();
    apply_tar_source_expansion(metadata, expansion);
    selected_shiny_entry
}

/// Reuse a prior record's derived expansion when its syntax and effective path
/// context are identical.
///
/// This is the only cache-like seam in the module: ownership and freshness stay
/// with the caller's authoritative record. Returns `true` when reuse occurred.
pub fn reuse_tar_source_expansion(
    metadata: &mut CrossFileMetadata,
    previous: &CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
) -> bool {
    if metadata.tar_source_requests != previous.tar_source_requests
        || metadata.list_files_source_requests != previous.list_files_source_requests
    {
        return false;
    }
    if previous.has_source_batch_topology() && previous.tar_source_expansion_watch_paths.is_empty()
    {
        return false;
    }
    metadata.shiny_application = previous.shiny_application.clone();
    metadata.application_working_directory = previous.application_working_directory.clone();
    let current_context = PathContext::from_metadata(uri, metadata, workspace_root);
    let previous_context = PathContext::from_metadata(uri, previous, workspace_root);
    if current_context != previous_context {
        return false;
    }
    let expansion = TarSourceExpansion {
        sources: previous
            .sources
            .iter()
            .filter(|source| source.is_source_batch_member())
            .cloned()
            .collect(),
        watch_paths: previous.tar_source_expansion_watch_paths.clone(),
        shiny_application: previous.shiny_application.clone(),
        application_working_directory: previous
            .application_working_directory
            .as_ref()
            .map(PathBuf::from),
        selected_shiny_entry: None,
    };
    apply_tar_source_expansion(metadata, expansion);
    true
}

/// Whether either path is equal to or lexically contains the other.
///
/// Matching is symmetric because a file event below a requested directory and
/// a directory event above a requested file must both select the parent.
pub fn paths_overlap(left: &Path, right: &Path) -> bool {
    if left == right || left.starts_with(right) || right.starts_with(left) {
        return true;
    }
    let left_components = left.components().count();
    let right_components = right.components().count();
    if left_components >= right_components {
        path_starts_with_ascii_case(left, right)
    } else {
        path_starts_with_ascii_case(right, left)
    }
}

fn path_starts_with_ascii_case(path: &Path, base: &Path) -> bool {
    let mut path_components = path.components();
    base.components().all(|base_component| {
        path_components.next().is_some_and(|path_component| {
            path_components_eq_ignore_ascii_case(path_component, base_component)
        })
    })
}

/// Compare path components with the same ASCII case leniency as watch overlap.
///
/// Windows canonicalization commonly changes `C:` into the extended-length
/// `\\?\C:` prefix (and likewise for UNC paths). Those prefix forms identify the
/// same filesystem root and must compare equal so canonical Shiny watch roots
/// can match ordinary file-URI event paths.
pub(crate) fn path_components_eq_ignore_ascii_case(
    left: std::path::Component<'_>,
    right: std::path::Component<'_>,
) -> bool {
    #[cfg(windows)]
    if let (std::path::Component::Prefix(left), std::path::Component::Prefix(right)) = (left, right)
    {
        use std::path::Prefix;

        return match (left.kind(), right.kind()) {
            (Prefix::Disk(left), Prefix::VerbatimDisk(right))
            | (Prefix::VerbatimDisk(left), Prefix::Disk(right))
            | (Prefix::Disk(left), Prefix::Disk(right))
            | (Prefix::VerbatimDisk(left), Prefix::VerbatimDisk(right)) => {
                left.eq_ignore_ascii_case(&right)
            }
            (
                Prefix::UNC(left_server, left_share),
                Prefix::VerbatimUNC(right_server, right_share),
            )
            | (
                Prefix::VerbatimUNC(left_server, left_share),
                Prefix::UNC(right_server, right_share),
            )
            | (Prefix::UNC(left_server, left_share), Prefix::UNC(right_server, right_share))
            | (
                Prefix::VerbatimUNC(left_server, left_share),
                Prefix::VerbatimUNC(right_server, right_share),
            ) => {
                os_str_eq_ignore_ascii_case(left_server, right_server)
                    && os_str_eq_ignore_ascii_case(left_share, right_share)
            }
            _ => os_str_eq_ignore_ascii_case(left.as_os_str(), right.as_os_str()),
        };
    }

    os_str_eq_ignore_ascii_case(left.as_os_str(), right.as_os_str())
}

#[cfg(unix)]
fn os_str_eq_ignore_ascii_case(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    left.as_bytes().eq_ignore_ascii_case(right.as_bytes())
}

#[cfg(windows)]
fn os_str_eq_ignore_ascii_case(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let mut left = left.encode_wide();
    let mut right = right.encode_wide();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(a), Some(b))
                if u8::try_from(a)
                    .ok()
                    .zip(u8::try_from(b).ok())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(&b))
                    || a == b => {}
            _ => return false,
        }
    }
}

/// Return existing and potential paths watched by the requests.
///
/// This full helper may perform case-correcting filesystem resolution.
pub fn tar_source_watch_paths(
    metadata: &CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
) -> Vec<PathBuf> {
    let mut paths = tar_source_lexical_watch_paths(metadata, uri, workspace_root);
    let Some(context) = PathContext::from_metadata(uri, metadata, workspace_root) else {
        return paths;
    };
    for request in &metadata.tar_source_requests {
        for raw in &request.files {
            if let Some(path) = resolve_path_with_workspace_fallback(raw, &context) {
                paths.push(path);
            }
        }
    }
    for request in &metadata.list_files_source_requests {
        if let Some(path) = resolve_path_with_workspace_fallback(&request.directory, &context) {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Return stat-free lexical and previously published watch paths.
pub(crate) fn tar_source_lexical_watch_paths(
    metadata: &CrossFileMetadata,
    uri: &Url,
    workspace_root: Option<&Url>,
) -> Vec<PathBuf> {
    let Some(context) = PathContext::from_metadata(uri, metadata, workspace_root) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for request in &metadata.tar_source_requests {
        for raw in &request.files {
            paths.extend(forward_path_candidate_tiers(raw, &context));
        }
    }
    for request in &metadata.list_files_source_requests {
        paths.extend(forward_path_candidate_tiers(&request.directory, &context));
    }
    paths.extend(metadata.tar_source_expansion_watch_paths.iter().cloned());
    // Preserve the previous successful spelling until an authoritative
    // replacement commits. This catches a deletion after a case-lenient or
    // symlinked resolution.
    paths.extend(
        metadata
            .sources
            .iter()
            .filter(|source| source.is_source_batch_member())
            .filter_map(|source| source.resolved_uri.as_ref())
            .filter_map(|uri| uri.to_file_path().ok()),
    );
    paths.sort();
    paths.dedup();
    paths
}

fn expand_request(request: &TarSourceRequest, context: &PathContext) -> RequestExpansion {
    let mut expansion = RequestExpansion::default();
    let mut seen = HashSet::new();
    for raw in &request.files {
        let candidates = candidate_watch_paths(raw, context);
        expansion.watch_paths.extend(candidates.iter().cloned());
        for candidate in &candidates {
            if std::fs::symlink_metadata(candidate)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                record_symlink_watch_paths(candidate, &mut expansion.watch_paths);
            }
        }
        let Some(path) = resolve_path_with_workspace_fallback(raw, context) else {
            continue;
        };
        if path.is_file() {
            push_r_file(&mut expansion.files, &mut seen, path);
        } else if path.is_dir() {
            let entries = walkdir::WalkDir::new(&path)
                .follow_links(true)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
                });
            let mut directory_files = Vec::new();
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        if let Some(path) = error.path() {
                            record_symlink_watch_paths(path, &mut expansion.watch_paths);
                        }
                        continue;
                    }
                };
                if entry.path_is_symlink() {
                    record_symlink_watch_paths(entry.path(), &mut expansion.watch_paths);
                }
                if entry.file_type().is_file() {
                    directory_files.push(entry.into_path());
                }
            }
            // Deterministic LC_COLLATE=C approximation over full relative paths.
            directory_files.sort_by_cached_key(|file| relative_full_path_key(&path, file));
            for file in directory_files {
                push_r_file(&mut expansion.files, &mut seen, file);
            }
        }
    }
    expansion.watch_paths.sort();
    expansion.watch_paths.dedup();
    expansion
}

fn expand_list_files_request(
    request: &ListFilesSourceRequest,
    context: &PathContext,
    workspace_root: Option<&Url>,
) -> RequestExpansion {
    expand_list_files_request_with_probe(request, context, workspace_root, |path| {
        std::fs::File::open(path).is_ok()
    })
}

fn expand_list_files_request_with_probe(
    request: &ListFilesSourceRequest,
    context: &PathContext,
    workspace_root: Option<&Url>,
    can_open: impl Fn(&Path) -> bool,
) -> RequestExpansion {
    let mut expansion = RequestExpansion::default();
    let candidates = candidate_watch_paths(&request.directory, context);
    expansion.watch_paths.extend(candidates.iter().cloned());
    for candidate in &candidates {
        if std::fs::symlink_metadata(candidate)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            record_symlink_watch_paths(candidate, &mut expansion.watch_paths);
        }
    }
    let outcome = resolve_source_path_rich(&request.directory, context);
    if outcome.case_mismatch == Some(CaseMismatchRegime::CaseSensitiveFs) {
        return expansion;
    }
    let Some(directory) = outcome.path else {
        return expansion;
    };
    if !directory.is_dir() || !path_is_within_workspace(&directory, workspace_root) {
        return expansion;
    }
    record_symlink_watch_paths(&directory, &mut expansion.watch_paths);
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => return expansion,
    };
    let mut files = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            expansion.files.clear();
            return expansion;
        };
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.')
            || Path::new(&name)
                .extension()
                .and_then(|value| value.to_str())
                != Some("R")
        {
            continue;
        }
        let path = entry.path();
        let Ok(symlink_metadata) = std::fs::symlink_metadata(&path) else {
            expansion.files.clear();
            return expansion;
        };
        if symlink_metadata.file_type().is_symlink() {
            record_symlink_watch_paths(&path, &mut expansion.watch_paths);
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            expansion.files.clear();
            return expansion;
        };
        if !metadata.is_file()
            || !path_is_within_workspace(&path, workspace_root)
            || !can_open(&path)
        {
            expansion.files.clear();
            return expansion;
        }
        files.push(path);
        if files.len() > MAX_LIST_FILES_SOURCE_MEMBERS {
            expansion.files.clear();
            return expansion;
        }
    }
    files.sort_by_cached_key(|file| relative_full_path_key(&directory, file));
    if files.iter().any(|file| path_to_uri(file).is_none()) {
        return expansion;
    }
    expansion.files = files;
    expansion.watch_paths.sort();
    expansion.watch_paths.dedup();
    expansion
}

fn path_is_within_workspace(path: &Path, workspace_root: Option<&Url>) -> bool {
    let Some(root) = workspace_root.and_then(|uri| uri.to_file_path().ok()) else {
        return false;
    };
    let Ok(resolved) = path.canonicalize() else {
        return false;
    };
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    resolved.starts_with(root)
}

/// Retain both lexical and canonical target spellings of a symlink.
fn record_symlink_watch_paths(path: &Path, watch_paths: &mut Vec<PathBuf>) {
    if let Ok(target) = std::fs::read_link(path) {
        let target = if target.is_absolute() {
            target
        } else {
            let lexical_parent = path.parent().unwrap_or_else(|| Path::new(""));
            let lexical_target = lexical_parent.join(&target);
            watch_paths.push(normalize_path_public(&lexical_target).unwrap_or(lexical_target));
            let resolved_parent = lexical_parent
                .canonicalize()
                .unwrap_or_else(|_| lexical_parent.to_path_buf());
            resolved_parent.join(target)
        };
        watch_paths.push(normalize_path_public(&target).unwrap_or(target));
    }
    if let Ok(target) = std::fs::canonicalize(path) {
        watch_paths.push(target);
    }
}

/// Rebase expanded tar children when one document has multiple graph roots.
pub fn remap_tar_sources_for_graph_root(
    root_meta: &mut CrossFileMetadata,
    input_root: &Url,
    input_meta: &CrossFileMetadata,
    root: &Url,
    workspace_root: Option<&Url>,
) {
    let Some(input_context) = PathContext::from_metadata(input_root, input_meta, workspace_root)
    else {
        return;
    };
    let Some(root_context) = PathContext::from_metadata(root, root_meta, workspace_root) else {
        return;
    };
    let input_base = input_context.effective_working_directory();
    let root_base = root_context.effective_working_directory();
    if input_base == root_base {
        return;
    }

    for source in &mut root_meta.sources {
        if !source.is_tar_source_member() {
            continue;
        }
        let Some(path) = source
            .resolved_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
        else {
            continue;
        };
        let relative = path
            .strip_prefix(&input_base)
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                let request = input_meta.tar_source_requests.iter().find(|request| {
                    request.line == source.line && request.column == source.column
                })?;
                request.files.iter().find_map(|raw| {
                    let raw_path = Path::new(raw);
                    if raw_path.is_absolute()
                        || !raw_path
                            .components()
                            .any(|component| component == std::path::Component::ParentDir)
                    {
                        return None;
                    }
                    let old_target = normalize_path_public(&input_base.join(raw_path))?;
                    if path == old_target {
                        return Some(raw_path.to_path_buf());
                    }
                    Some(raw_path.join(path.strip_prefix(old_target).ok()?))
                })
            });
        let Some(relative) = relative else {
            continue;
        };
        let Some(rebased) = normalize_path_public(&root_base.join(relative)) else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(&rebased) else {
            continue;
        };
        source.path = rebased.to_string_lossy().into_owned();
        source.resolved_uri = Some(uri);
    }
}

#[cfg(unix)]
fn relative_full_path_key(root: &Path, file: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    file.strip_prefix(root)
        .unwrap_or(file)
        .as_os_str()
        .as_bytes()
        .to_vec()
}

#[cfg(windows)]
fn relative_full_path_key(root: &Path, file: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let relative = file.strip_prefix(root).unwrap_or(file);
    let mut key = Vec::new();
    for (index, component) in relative.components().enumerate() {
        if index > 0 {
            key.push(u16::from(b'/'));
        }
        key.extend(component.as_os_str().encode_wide());
    }
    key
}

fn candidate_watch_paths(raw: &str, context: &PathContext) -> Vec<PathBuf> {
    let mut paths = forward_path_candidate_tiers(raw, context);
    if let Some(path) = resolve_path_with_workspace_fallback(raw, context) {
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn push_r_file(files: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    let is_r = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "R" | "r"));
    if is_r && seen.insert(path.clone()) {
        files.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn finalize(root: &Path, parent: &Path, code: &str) -> CrossFileMetadata {
        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(parent).unwrap();
        let mut metadata = crate::cross_file::extract_metadata(code);
        finalize_tar_source_requests(&mut metadata, &parent_uri, Some(&root_uri));
        metadata
    }

    fn relative_sources(root: &Path, metadata: &CrossFileMetadata) -> Vec<String> {
        metadata
            .sources
            .iter()
            .filter(|source| source.tar_source_ordinal.is_some())
            .map(|source| {
                source
                    .resolved_uri
                    .as_ref()
                    .unwrap()
                    .to_file_path()
                    .unwrap()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn list_files_loop_expands_immediate_uppercase_r_members_in_c_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("functions/b.R"), "b <- 1\n");
        touch(&root.join("functions/a.R"), "a <- 1\n");
        touch(&root.join("functions/lower.r"), "lower <- 1\n");
        touch(&root.join("functions/.hidden.R"), "hidden <- 1\n");
        touch(&root.join("functions/nested/deep.R"), "deep <- 1\n");
        let parent = root.join("main.R");
        let code = "files <- list.files(\"functions\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
                    for (file in files) source(file)\n";
        touch(&parent, code);

        let metadata = finalize(root, &parent, code);
        assert_eq!(
            relative_sources(root, &metadata),
            ["functions/a.R", "functions/b.R"]
        );
        assert!(
            metadata.sources.iter().all(|source| {
                source.source_batch_kind == Some(SourceBatchKind::ListFiles)
                    && !source.is_tar_source_member()
            }),
            "{:?}",
            metadata.sources
        );
        assert_eq!(
            metadata
                .sources
                .iter()
                .filter_map(|source| source.tar_source_ordinal)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn missing_list_files_directory_remains_watchable_then_refreshes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = root.join("main.R");
        let code = "files <- list.files(\"functions\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
                    for (file in files) source(file)\n";
        touch(&parent, code);
        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let mut metadata = crate::cross_file::extract_metadata(code);

        finalize_tar_source_requests(&mut metadata, &parent_uri, Some(&root_uri));
        assert!(metadata.sources.is_empty());
        assert!(
            metadata
                .tar_source_expansion_watch_paths
                .contains(&root.join("functions"))
        );

        touch(&root.join("functions/later.R"), "later <- 1\n");
        finalize_tar_source_requests(&mut metadata, &parent_uri, Some(&root_uri));
        assert_eq!(relative_sources(root, &metadata), ["functions/later.R"]);
    }

    #[test]
    fn matching_directory_drops_the_whole_list_files_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("functions/good.R"), "good <- 1\n");
        fs::create_dir_all(root.join("functions/not-a-file.R")).unwrap();
        let parent = root.join("main.R");
        let code = "files <- list.files(\"functions\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
                    for (file in files) source(file)\n";
        touch(&parent, code);

        let metadata = finalize(root, &parent, code);
        assert!(
            metadata.sources.is_empty(),
            "a partial prefix would model the wrong execution: {:?}",
            metadata.sources
        );
    }

    #[test]
    fn unopenable_member_drops_the_whole_list_files_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("functions/good.R"), "good <- 1\n");
        touch(&root.join("functions/unopenable.R"), "unopenable <- 1\n");
        let parent = root.join("main.R");
        let code = "files <- list.files(\"functions\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
                    for (file in files) source(file)\n";
        touch(&parent, code);
        let workspace_root = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let metadata = crate::cross_file::extract_metadata(code);
        let context =
            PathContext::from_metadata(&parent_uri, &metadata, Some(&workspace_root)).unwrap();

        let expansion = expand_list_files_request_with_probe(
            &metadata.list_files_source_requests[0],
            &context,
            Some(&workspace_root),
            |path| path.file_name().and_then(|name| name.to_str()) != Some("unopenable.R"),
        );

        assert!(
            expansion.files.is_empty(),
            "an unreadable member must reject the entire batch: {:?}",
            expansion.files
        );
    }

    #[test]
    fn list_files_directory_case_follows_runtime_existence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("Functions/member.R"), "member <- 1\n");
        let typed_directory = root.join("functions");
        let parent = root.join("main.R");
        let code = "files <- list.files(\"functions\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
                    for (file in files) source(file)\n";
        touch(&parent, code);

        let metadata = finalize(root, &parent, code);
        assert_eq!(
            metadata.sources.len(),
            usize::from(typed_directory.exists()),
            "a wrong-case directory executes only when that spelling exists on this host"
        );
    }

    #[test]
    fn list_files_member_cap_drops_the_whole_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for index in 0..=MAX_LIST_FILES_SOURCE_MEMBERS {
            touch(
                &root.join(format!("functions/member-{index:03}.R")),
                "value <- 1\n",
            );
        }
        let parent = root.join("main.R");
        let code = "files <- list.files(\"functions\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
                    for (file in files) source(file)\n";
        touch(&parent, code);

        let metadata = finalize(root, &parent, code);
        assert!(metadata.sources.is_empty());
    }

    #[test]
    fn list_files_without_workspace_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("functions/member.R"), "member <- 1\n");
        let parent = root.join("main.R");
        let directory = root
            .join("functions")
            .to_string_lossy()
            .replace('\\', "\\\\");
        let code = format!(
            "files <- list.files(\"{directory}\", pattern = \"\\\\.R$\", full.names = TRUE)\n\
             for (file in files) source(file)\n"
        );
        touch(&parent, &code);
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let mut metadata = crate::cross_file::extract_metadata(&code);

        finalize_tar_source_requests(&mut metadata, &parent_uri, None);

        assert!(metadata.sources.is_empty());
    }

    #[test]
    fn expands_mixed_inputs_recursively_with_per_call_dedup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("R/b.R"), "");
        touch(&root.join("R/nested/a.r"), "");
        touch(&root.join("R/skip.txt"), "");
        touch(&root.join("setup.R"), "");
        let parent = root.join("_targets.R");
        touch(&parent, "");

        let metadata = finalize(
            root,
            &parent,
            "targets::tar_source(c(\"setup.R\", \"R\", \"setup.R\"))",
        );
        assert_eq!(
            relative_sources(root, &metadata),
            ["setup.R", "R/b.R", "R/nested/a.r"]
        );
        assert!(
            metadata
                .sources
                .iter()
                .filter_map(|source| source.tar_source_ordinal)
                .eq(0..3)
        );
    }

    #[test]
    fn excludes_hidden_entries_but_allows_explicit_hidden_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("R/a.R"), "");
        touch(&root.join("R/.hidden.R"), "");
        touch(&root.join("R/.hidden/z.R"), "");
        touch(&root.join(".scripts/explicit.R"), "");
        let parent = root.join("_targets.R");
        touch(&parent, "");

        let metadata = finalize(root, &parent, "targets::tar_source(c(\"R\", \".scripts\"))");
        assert_eq!(
            relative_sources(root, &metadata),
            ["R/a.R", ".scripts/explicit.R"]
        );
    }

    #[test]
    fn deduplicates_per_call_but_not_across_calls() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("a.R"), "");
        let parent = root.join("_targets.R");
        touch(&parent, "");
        let metadata = finalize(
            root,
            &parent,
            "targets::tar_source(c(\"a.R\", \"a.R\"))\ntargets::tar_source(\"a.R\")",
        );
        assert_eq!(relative_sources(root, &metadata), ["a.R", "a.R"]);
        assert_eq!(
            metadata
                .sources
                .iter()
                .filter_map(|source| source.tar_source_ordinal)
                .collect::<Vec<_>>(),
            [0, 0]
        );
    }

    #[test]
    fn missing_request_remains_watchable_then_refreshes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = root.join("_targets.R");
        touch(&parent, "");
        let code = "targets::tar_source(\"later\")";
        let mut metadata = finalize(root, &parent, code);
        assert!(relative_sources(root, &metadata).is_empty());
        assert_eq!(metadata.tar_source_requests.len(), 1);
        assert!(
            tar_source_watch_paths(
                &metadata,
                &Url::from_file_path(&parent).unwrap(),
                Some(&Url::from_directory_path(root).unwrap())
            )
            .iter()
            .any(|path| path.ends_with("later"))
        );

        touch(&root.join("later/new.R"), "");
        finalize_tar_source_requests(
            &mut metadata,
            &Url::from_file_path(&parent).unwrap(),
            Some(&Url::from_directory_path(root).unwrap()),
        );
        assert_eq!(relative_sources(root, &metadata), ["later/new.R"]);
    }

    #[test]
    fn expansion_uses_workspace_explicit_inherited_and_implicit_forward_tiers() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("R/workspace.R"), "workspace <- 1\n");
        touch(&root.join("runtime/R/runtime.R"), "runtime <- 1\n");
        touch(
            &root.join("tests/testthat/R/test-helper.R"),
            "test_helper <- 1\n",
        );

        let workspace_parent = root.join("scripts/_targets.R");
        touch(&workspace_parent, "targets::tar_source(\"R\")\n");
        assert_eq!(
            relative_sources(
                root,
                &finalize(root, &workspace_parent, "targets::tar_source(\"R\")\n")
            ),
            ["R/workspace.R"]
        );

        let explicit_parent = root.join("scripts/explicit.R");
        let explicit_code = "# raven: cd ../runtime\ntargets::tar_source(\"R\")\n";
        touch(&explicit_parent, explicit_code);
        assert_eq!(
            relative_sources(root, &finalize(root, &explicit_parent, explicit_code)),
            ["runtime/R/runtime.R"]
        );

        let inherited_parent = root.join("scripts/inherited.R");
        let inherited_code = "targets::tar_source(\"R\")\n";
        touch(&inherited_parent, inherited_code);
        let inherited_uri = Url::from_file_path(&inherited_parent).unwrap();
        let root_uri = Url::from_directory_path(root).unwrap();
        let mut inherited = crate::cross_file::extract_metadata(inherited_code);
        inherited.inherited_working_directory =
            Some(root.join("runtime").to_string_lossy().into_owned());
        finalize_tar_source_requests(&mut inherited, &inherited_uri, Some(&root_uri));
        assert_eq!(relative_sources(root, &inherited), ["runtime/R/runtime.R"]);

        let implicit_parent = root.join("tests/testthat/test-parent.R");
        touch(&implicit_parent, inherited_code);
        assert_eq!(
            relative_sources(root, &finalize(root, &implicit_parent, inherited_code)),
            ["tests/testthat/R/test-helper.R"]
        );
    }

    #[test]
    fn reuse_requires_identical_requests_and_path_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("R/a.R"), "");
        let parent = root.join("_targets.R");
        touch(&parent, "");
        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let previous = finalize(root, &parent, "targets::tar_source(\"R\")");

        let mut same = crate::cross_file::extract_metadata("targets::tar_source(\"R\")");
        assert!(reuse_tar_source_expansion(
            &mut same,
            &previous,
            &parent_uri,
            Some(&root_uri)
        ));
        assert_eq!(relative_sources(root, &same), ["R/a.R"]);

        let mut changed = crate::cross_file::extract_metadata(
            "# raven: cd elsewhere\ntargets::tar_source(\"R\")",
        );
        assert!(!reuse_tar_source_expansion(
            &mut changed,
            &previous,
            &parent_uri,
            Some(&root_uri)
        ));
    }

    #[test]
    fn direct_shiny_helper_expansion_retains_selected_host_without_emitting_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("app.R"), "entry_value <- helper_value\n");
        touch(&root.join("R/a.R"), "helper_value <- 1\n");
        touch(&root.join("R/b.R"), "later_value <- 2\n");
        let helper_uri = Url::from_file_path(root.join("R/b.R")).unwrap();
        let root_uri = Url::from_directory_path(root).unwrap();
        let metadata = crate::cross_file::extract_metadata("later_value <- 2\n");

        let expansion = expand_tar_source_requests_with_exclusions(
            &metadata,
            &helper_uri,
            Some(&root_uri),
            &CompiledWorkspaceExclusions::default(),
        );

        assert_eq!(
            expansion.selected_shiny_entry,
            Some(root.join("app.R").canonicalize().unwrap())
        );
        assert!(expansion.sources.is_empty());
        assert_eq!(
            expansion.shiny_application.unwrap().role,
            super::super::types::ShinyFileRole::Helper { ordinal: 1 }
        );
        assert_eq!(
            expansion.application_working_directory,
            Some(root.to_path_buf())
        );
    }

    #[test]
    fn overlap_is_symmetric_and_ascii_case_lenient() {
        assert!(paths_overlap(
            Path::new("/workspace/R/child.R"),
            Path::new("/workspace/r")
        ));
        assert!(paths_overlap(
            Path::new("/workspace"),
            Path::new("/workspace/R/child.R")
        ));
        assert!(!paths_overlap(
            Path::new("/workspace/R"),
            Path::new("/workspace/other")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn overlap_equates_normal_and_verbatim_windows_roots() {
        assert!(paths_overlap(
            Path::new(r"C:\real-app\R\helper.R"),
            Path::new(r"\\?\c:\REAL-APP")
        ));
        assert!(paths_overlap(
            Path::new(r"\\server\share\real-app\app.R"),
            Path::new(r"\\?\UNC\SERVER\SHARE\real-app")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn relative_path_order_uses_raw_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = Path::new("/workspace");
        let lower = root.join(OsString::from_vec(b"a\x80.R".to_vec()));
        let higher = root.join(OsString::from_vec(b"a\xff.R".to_vec()));
        assert!(relative_full_path_key(root, &lower) < relative_full_path_key(root, &higher));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_directories_and_broken_targets_remain_watchable() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("real.R"), "real <- 1\n");
        touch(&root.join("real-dir/nested.R"), "nested <- 1\n");
        symlink("real.R", root.join("linked.R")).unwrap();
        symlink("real-dir", root.join("linked-dir")).unwrap();
        symlink("missing-target.R", root.join("broken.R")).unwrap();
        let parent = root.join("_targets.R");
        let code = "targets::tar_source(c(\"linked.R\", \"linked-dir\", \"broken.R\"))\n";
        touch(&parent, code);

        let metadata = finalize(root, &parent, code);
        assert_eq!(
            metadata
                .sources
                .iter()
                .filter(|source| source.tar_source_ordinal.is_some())
                .count(),
            2
        );
        assert!(
            metadata
                .tar_source_expansion_watch_paths
                .contains(&root.join("missing-target.R")),
            "broken symlink target spelling must remain watchable"
        );
        assert!(
            metadata
                .tar_source_expansion_watch_paths
                .contains(&root.join("real-dir").canonicalize().unwrap()),
            "directory symlink target must remain watchable"
        );
    }

    #[test]
    fn contextual_providers_discover_tar_batch_below_ordinary_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("main.R"), "source(\"driver.R\")\n");
        let driver_code = "# raven: cd runtime\n\
                           targets::tar_source(\"../child.R\", change_directory = FALSE)\n\
                           targets::tar_source(\"../child.R\", change_directory = TRUE)\n";
        touch(&root.join("driver.R"), driver_code);
        touch(&root.join("child.R"), "source(\"config.R\")\n");
        touch(&root.join("runtime/config.R"), "runtime_symbol <- 1\n");
        touch(&root.join("config.R"), "root_symbol <- 1\n");

        let root_uri = Url::from_directory_path(root).unwrap();
        let main_uri = Url::from_file_path(root.join("main.R")).unwrap();
        let driver_uri = Url::from_file_path(root.join("driver.R")).unwrap();
        let child_uri = Url::from_file_path(root.join("child.R")).unwrap();
        let runtime_uri = Url::from_file_path(root.join("runtime/config.R")).unwrap();
        let config_uri = Url::from_file_path(root.join("config.R")).unwrap();
        let main_metadata = crate::cross_file::extract_metadata("source(\"driver.R\")\n");
        let driver_metadata = finalize(root, &root.join("driver.R"), driver_code);
        let metadata = HashMap::from([
            (driver_uri.clone(), std::sync::Arc::new(driver_metadata)),
            (
                child_uri.clone(),
                std::sync::Arc::new(crate::cross_file::extract_metadata(
                    "source(\"config.R\")\n",
                )),
            ),
            (
                runtime_uri.clone(),
                std::sync::Arc::new(crate::cross_file::extract_metadata("runtime_symbol <- 1\n")),
            ),
            (
                config_uri.clone(),
                std::sync::Arc::new(crate::cross_file::extract_metadata("root_symbol <- 1\n")),
            ),
        ]);
        let mut graph = crate::cross_file::dependency::DependencyGraph::new();
        graph.update_file(&main_uri, &main_metadata, Some(&root_uri), |_| None);
        for uri in [&driver_uri, &child_uri, &runtime_uri, &config_uri] {
            graph.update_file(uri, metadata[uri].as_ref(), Some(&root_uri), |_| None);
        }
        let edge_revision = graph.edge_revision();
        let main_edges = graph.get_dependencies(&main_uri);

        let collected = collect_contextual_tar_providers(
            &main_uri,
            &main_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|parent, source| {
                let target = graph
                    .get_dependencies(parent)
                    .into_iter()
                    .find(|edge| {
                        edge.call_site_line == Some(source.line)
                            && edge.call_site_column == Some(source.column)
                            && edge.tar_source_ordinal == source.tar_source_ordinal
                            && edge.source_batch_kind == source.source_batch_kind
                    })
                    .map(|edge| edge.to.clone());
                GraphPrefixEdgeLookup::Known(target)
            },
        );
        assert!(collected.divergence);
        assert!(collected.providers.contains(&runtime_uri));
        assert!(collected.providers.contains(&config_uri));
        assert!(
            collected
                .executions
                .iter()
                .any(|execution| execution.uri == driver_uri && !execution.contextual)
        );
        assert_eq!(graph.edge_revision(), edge_revision);
        assert_eq!(graph.get_dependencies(&main_uri), main_edges);
    }

    #[test]
    fn graph_prefix_unknown_edges_fail_closed_without_lexical_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("main.R"), "source(\"driver.R\")\n");
        touch(&root.join("driver.R"), "targets::tar_source(\"child.R\")\n");
        touch(&root.join("child.R"), "member <- 1\n");
        let root_uri = Url::from_directory_path(root).unwrap();
        let main_uri = Url::from_file_path(root.join("main.R")).unwrap();
        let main_metadata = crate::cross_file::extract_metadata("source(\"driver.R\")\n");

        let unknown = collect_contextual_tar_providers(
            &main_uri,
            &main_metadata,
            Some(&root_uri),
            10,
            100,
            &|_| None,
            &|_, _| GraphPrefixEdgeLookup::Unknown,
        );
        assert!(unknown.truncated);
        assert!(unknown.divergence);
        assert!(
            unknown.providers.is_empty(),
            "an unknown graph prefix must not be replaced with the lexically existing driver"
        );

        let known_unresolved = collect_contextual_tar_providers(
            &main_uri,
            &main_metadata,
            Some(&root_uri),
            10,
            100,
            &|_| None,
            &|_, _| GraphPrefixEdgeLookup::Known(None),
        );
        assert!(!known_unresolved.truncated);
        assert!(!known_unresolved.divergence);
        assert!(known_unresolved.providers.is_empty());
    }

    #[test]
    fn graph_prefix_follows_non_inheriting_edges_to_tar_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let main_code = "sys.source(\"driver.R\", envir = new.env())\n";
        touch(&root.join("main.R"), main_code);
        let driver_code = "targets::tar_source(\"child.R\")\n";
        touch(&root.join("driver.R"), driver_code);
        touch(&root.join("child.R"), "member <- 1\n");

        let root_uri = Url::from_directory_path(root).unwrap();
        let main_uri = Url::from_file_path(root.join("main.R")).unwrap();
        let driver_uri = Url::from_file_path(root.join("driver.R")).unwrap();
        let child_uri = Url::from_file_path(root.join("child.R")).unwrap();
        let main_metadata = crate::cross_file::extract_metadata(main_code);
        assert_eq!(
            main_metadata.sources[0].locality,
            SourceLocality::NonInheriting,
            "test premise: a new.env() sys.source must be NonInheriting"
        );
        let driver_metadata = finalize(root, &root.join("driver.R"), driver_code);
        let metadata = HashMap::from([
            (driver_uri.clone(), std::sync::Arc::new(driver_metadata)),
            (
                child_uri.clone(),
                std::sync::Arc::new(crate::cross_file::extract_metadata("member <- 1\n")),
            ),
        ]);
        let mut graph = crate::cross_file::dependency::DependencyGraph::new();
        graph.update_file(&main_uri, &main_metadata, Some(&root_uri), |_| None);
        for uri in [&driver_uri, &child_uri] {
            graph.update_file(uri, metadata[uri].as_ref(), Some(&root_uri), |_| None);
        }

        let collected = collect_contextual_tar_providers(
            &main_uri,
            &main_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|parent, source| {
                let target = graph
                    .get_dependencies(parent)
                    .into_iter()
                    .find(|edge| {
                        edge.call_site_line == Some(source.line)
                            && edge.call_site_column == Some(source.column)
                            && edge.tar_source_ordinal == source.tar_source_ordinal
                            && edge.source_batch_kind == source.source_batch_kind
                    })
                    .map(|edge| edge.to.clone());
                GraphPrefixEdgeLookup::Known(target)
            },
        );
        assert!(
            collected.providers.contains(&child_uri),
            "a NonInheriting graph-prefix edge must still host the driver's tar batch: {collected:?}"
        );
        assert!(
            collected
                .executions
                .iter()
                .any(|execution| execution.uri == driver_uri && !execution.contextual),
            "{collected:?}"
        );
        assert!(!collected.divergence);
        assert!(!collected.truncated);

        // Locality-agnostic traversal must not weaken fail-closed behavior: an
        // Unknown graph view still yields no providers through the same edge.
        let unknown = collect_contextual_tar_providers(
            &main_uri,
            &main_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|_, _| GraphPrefixEdgeLookup::Unknown,
        );
        assert!(unknown.truncated);
        assert!(unknown.divergence);
        assert!(unknown.providers.is_empty());
    }

    #[test]
    fn contextual_closure_follows_non_inheriting_edges_for_process_effects() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = root.join("_targets.R");
        let parent_code = "targets::tar_source(\"child.R\")\n";
        touch(&parent, parent_code);
        let child_code = "sys.source(\"helper.R\", envir = new.env())\n";
        touch(&root.join("child.R"), child_code);
        let helper_code = "library(syntheticpkg)\nprivate_helper <- 1\n";
        touch(&root.join("helper.R"), helper_code);

        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let child_uri = Url::from_file_path(root.join("child.R")).unwrap();
        let helper_uri = Url::from_file_path(root.join("helper.R")).unwrap();
        let parent_metadata = finalize(root, &parent, parent_code);
        let child_metadata = crate::cross_file::extract_metadata(child_code);
        assert_eq!(
            child_metadata.sources[0].locality,
            SourceLocality::NonInheriting
        );
        let metadata = HashMap::from([
            (child_uri.clone(), std::sync::Arc::new(child_metadata)),
            (
                helper_uri.clone(),
                std::sync::Arc::new(crate::cross_file::extract_metadata(helper_code)),
            ),
        ]);

        let collected = collect_contextual_tar_providers(
            &parent_uri,
            &parent_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|_, _| GraphPrefixEdgeLookup::Known(None),
        );
        assert!(collected.providers.contains(&child_uri));
        assert!(
            collected.providers.contains(&helper_uri),
            "contextual traversal must retain a NonInheriting helper so scope can observe its \
             process-wide package/data effects: {collected:?}"
        );
        assert!(!collected.divergence);
        assert!(!collected.truncated);
    }

    #[test]
    fn contextual_providers_are_sorted_and_distinguish_repeated_child_contexts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("child.R"), "source(\"config.R\")\n");
        touch(&root.join("runtime/config.R"), "runtime <- 1\n");
        touch(&root.join("config.R"), "root <- 1\n");
        let parent = root.join("_targets.R");
        let code = "# raven: cd runtime\n\
                    targets::tar_source(\"../child.R\", change_directory = FALSE)\n\
                    targets::tar_source(\"../child.R\", change_directory = TRUE)\n";
        touch(&parent, code);
        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let child_uri = Url::from_file_path(root.join("child.R")).unwrap();
        let runtime_uri = Url::from_file_path(root.join("runtime/config.R")).unwrap();
        let config_uri = Url::from_file_path(root.join("config.R")).unwrap();
        let parent_metadata = finalize(root, &parent, code);
        let metadata = HashMap::from([(
            child_uri.clone(),
            std::sync::Arc::new(crate::cross_file::extract_metadata(
                "source(\"config.R\")\n",
            )),
        )]);
        let mut graph = crate::cross_file::dependency::DependencyGraph::new();
        graph.update_file(&parent_uri, &parent_metadata, Some(&root_uri), |_| None);
        graph.update_file(
            &child_uri,
            metadata[&child_uri].as_ref(),
            Some(&root_uri),
            |_| None,
        );
        let edge_revision = graph.edge_revision();
        let child_edges = graph.get_dependencies(&child_uri);

        let collected = collect_contextual_tar_providers(
            &parent_uri,
            &parent_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|_, _| GraphPrefixEdgeLookup::Known(None),
        );
        assert!(collected.divergence);
        assert!(!collected.truncated);
        assert!(collected.providers.contains(&runtime_uri));
        assert!(collected.providers.contains(&config_uri));
        assert!(
            collected
                .providers
                .windows(2)
                .all(|pair| pair[0].as_str() <= pair[1].as_str())
        );
        assert_eq!(graph.edge_revision(), edge_revision);
        assert_eq!(
            graph.get_dependencies(&child_uri),
            child_edges,
            "context collection must not add alternate graph edges"
        );
    }

    #[test]
    fn contextual_providers_keep_coincident_contexts_non_divergent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("child.R"), "source(\"config.R\")\n");
        touch(&root.join("runtime/config.R"), "runtime <- 1\n");
        let parent = root.join("_targets.R");
        let code = "# raven: cd runtime\n\
                    targets::tar_source(\"../child.R\", change_directory = FALSE)\n\
                    targets::tar_source(\"../child.R\", change_directory = FALSE)\n";
        touch(&parent, code);
        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let child_uri = Url::from_file_path(root.join("child.R")).unwrap();
        let runtime_uri = Url::from_file_path(root.join("runtime/config.R")).unwrap();
        let parent_metadata = finalize(root, &parent, code);
        let metadata = HashMap::from([
            (
                child_uri,
                std::sync::Arc::new(crate::cross_file::extract_metadata(
                    "source(\"config.R\")\n",
                )),
            ),
            (
                runtime_uri,
                std::sync::Arc::new(crate::cross_file::extract_metadata("runtime <- 1\n")),
            ),
        ]);

        let collected = collect_contextual_tar_providers(
            &parent_uri,
            &parent_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|_, _| GraphPrefixEdgeLookup::Known(None),
        );
        assert!(!collected.divergence);
        assert!(!collected.truncated);
    }

    #[test]
    fn contextual_provider_budget_and_cycle_terminate_safe_direction() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        touch(&root.join("a.R"), "source(\"a.R\")\n");
        touch(&root.join("b.R"), "b <- 1\n");
        let parent = root.join("_targets.R");
        let code = "targets::tar_source(c(\"a.R\", \"b.R\"))\n";
        touch(&parent, code);
        let root_uri = Url::from_directory_path(root).unwrap();
        let parent_uri = Url::from_file_path(&parent).unwrap();
        let a_uri = Url::from_file_path(root.join("a.R")).unwrap();
        let parent_metadata = finalize(root, &parent, code);
        let metadata = HashMap::from([(
            a_uri,
            std::sync::Arc::new(crate::cross_file::extract_metadata("source(\"a.R\")\n")),
        )]);

        let bounded = collect_contextual_tar_providers(
            &parent_uri,
            &parent_metadata,
            Some(&root_uri),
            10,
            1,
            &|uri| metadata.get(uri).cloned(),
            &|_, _| GraphPrefixEdgeLookup::Known(None),
        );
        assert!(bounded.truncated);
        assert!(bounded.divergence);

        let cyclic = collect_contextual_tar_providers(
            &parent_uri,
            &parent_metadata,
            Some(&root_uri),
            10,
            100,
            &|uri| metadata.get(uri).cloned(),
            &|_, _| GraphPrefixEdgeLookup::Known(None),
        );
        assert!(!cyclic.truncated);
        assert!(cyclic.providers.len() <= 2);
    }
}
