//! `raven check` subcommand: index a workspace, then report the full diagnostic
//! set (syntax, semantic, style lints, cross-file, package, and
//! undefined-variable) for the requested files — the same diagnostics the
//! editor publishes, run headlessly for CI.
//!
//! Unlike `raven lint` (style rules only; no `WorldState`, no cross-file), this
//! builds a real `WorldState`, runs the same workspace scan the LSP server runs
//! on startup, auto-detects R to populate the package library, then drives the
//! shared diagnostic pipeline per reported file:
//! `DiagnosticsSnapshot::build` → `handlers::diagnostics_from_snapshot` →
//! `handlers::diagnostics_async_standalone` (the same three steps as the LSP
//! publish path — notably NOT the `handlers::diagnostics()` convenience
//! wrapper, which skips the async on-disk missing-file checks).
//!
//! The whole workspace is always indexed so cross-file resolution is accurate;
//! `PATHS` only filter which files have their diagnostics *reported*.

use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Url};

use crate::cli::shared::{
    ColorChoice, EXIT_LINT_FAILED, EXIT_OK, EXIT_OPERATOR_ERROR, OutputFormat, SeverityLevel,
    absolute_path, collect_check_target_paths_with_exclusions, encoding_diagnostic, is_chunk_file,
    is_jags_file, is_r_file, is_stan_file, parse_color_choice, parse_output_format,
    parse_severity_level, render, resolve_color_from_env,
};

#[derive(Debug, PartialEq, Clone)]
pub struct CheckArgs {
    /// Files / directories to report on. Empty means every supported R or
    /// standalone model file in the workspace. These filter only what is
    /// *reported*; the whole workspace is always indexed regardless.
    pub paths: Vec<PathBuf>,
    /// Workspace root to index. Defaults to the current directory.
    pub workspace: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub no_config: bool,
    pub format: OutputFormat,
    pub max_severity: SeverityLevel,
    pub quiet: bool,
    /// Color control for `text` output. `--no-color` parses to
    /// [`ColorChoice::Never`]; `--color auto|always|never` sets it directly.
    /// Resolved to on/off by [`resolve_color_from_env`] (TTY +
    /// `NO_COLOR`/`FORCE_COLOR`).
    pub color: ColorChoice,
    /// Enable the missing-package ("not installed") diagnostic. Disabled by
    /// default because `raven check` often runs in environments without package
    /// installation. Reports `library()` calls absent from the local library
    /// paths — NOT relative to Tier 2/Tier 3 metadata (see docs/diagnostics.md).
    pub report_uninstalled: bool,
}

pub fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<CheckArgs, String> {
    let mut paths = Vec::new();
    let mut workspace = None;
    let mut config_path = None;
    let mut no_config = false;
    let mut format = OutputFormat::Text;
    let mut max_severity = SeverityLevel::Info;
    let mut quiet = false;
    // `--color` and `--no-color` write the same field; last-one-wins on conflict
    // (`--no-color --color always` ⇒ always), matching cargo/ripgrep.
    let mut color = ColorChoice::Auto;
    let mut report_uninstalled = false;

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--workspace" => {
                workspace = Some(PathBuf::from(
                    argv.next().ok_or("--workspace needs a path")?,
                ));
            }
            "--config" => {
                config_path = Some(PathBuf::from(argv.next().ok_or("--config needs a path")?));
            }
            "--no-config" => no_config = true,
            "--format" => {
                let v = argv.next().ok_or("--format needs a value")?;
                format = parse_output_format(&v)?;
            }
            "--max-severity" => {
                let v = argv.next().ok_or("--max-severity needs a value")?;
                max_severity = parse_severity_level(&v)?;
            }
            "--quiet" => quiet = true,
            "--color" => {
                let v = argv.next().ok_or("--color needs a value")?;
                color = parse_color_choice(&v)?;
            }
            "--no-color" => color = ColorChoice::Never,
            "--report-uninstalled" => report_uninstalled = true,
            "--help" => return Err("HELP".into()),
            s if s.starts_with("--") => return Err(format!("unknown flag: {s}")),
            p => paths.push(PathBuf::from(p)),
        }
    }
    Ok(CheckArgs {
        paths,
        workspace,
        config_path,
        no_config,
        format,
        max_severity,
        quiet,
        color,
        report_uninstalled,
    })
}

pub fn print_help() {
    println!(
        "raven check {} — R and model diagnostics for CI

Usage: raven check [OPTIONS] [PATHS...]

Indexes the workspace, then reports diagnostics for the requested files (or
every R / R Markdown / Quarto / JAGS / Stan file in the workspace when no PATHS
are given): syntax errors, semantic checks, style lints, cross-file diagnostics
(missing source files, circular dependencies, out-of-scope usage),
missing-package warnings, and undefined-variable diagnostics. For .Rmd /
.Rmarkdown / .qmd the R code inside chunks is analyzed; prose and non-R chunks
are ignored. JAGS `.jags` / `.bugs` / `.bug` and Stan `.stan` files are always
discovered, but their diagnostics are opt-in: set `jags = \"on\"` and/or `stan =
\"on\"` under `[diagnostics]` in raven.toml. JAGS receives native syntax checks;
complete Stan programs also receive conservative undeclared-variable checks.
`--no-config` therefore leaves model diagnostics disabled.
Honors raven.toml / .lintr.

Options:
  --workspace DIR             Workspace root to index (default: current directory)
  --config PATH               Path to raven.toml or .lintr (default: search upward from
                              --workspace, skipping literal ~/.lintr unless passed here)
  --no-config                 Use built-in defaults; ignore raven.toml/.lintr
  --format text|json|sarif    Output format (default: text)
  --max-severity LEVEL        Highest severity that does NOT fail the build
                              (off, hint, info, warning, error; default: info)
  --quiet                     Suppress summary line in text output
  --color auto|always|never   When to colorize text output (default: auto —
                              color when stdout is a terminal). Honors NO_COLOR
                              and FORCE_COLOR under auto; json/sarif are never
                              colorized.
  --no-color                  Alias for --color never

R / packages:
  raven check auto-detects R on PATH to resolve installed-package exports and
  base R symbols. If R is not found and no offline package data is present
  (`raven packages update`/`freeze`), package symbol resolution is limited to
  base R (still covered by the embedded database) and a note is printed to
  stderr; all other diagnostics still run.

  --report-uninstalled        Report packages from library() calls that are not
                              present in the local library paths. Disabled by
                              default; useful when the environment DID install
                              packages (e.g. renv::restore()) and you want to
                              catch failures.

Exit codes:
  0   No diagnostic exceeded --max-severity
  1   A diagnostic exceeded --max-severity, or a usage error (unknown flag / bad option value)
  2   Operator error while running (config parse failure, unreadable path)
",
        env!("CARGO_PKG_VERSION")
    );
}

/// Open a report target that the workspace scan did NOT index (a path reached
/// through a different symlink alias, OR a chunk file — `.Rmd`/`.Rmarkdown`/
/// `.qmd` are deliberately outside the R-only workspace scan) into
/// `state.documents` and wire its outgoing edges into the dependency graph.
///
/// `text` is the already-decoded file contents (the caller owns the
/// `read_source` error handling). `path` is used only to classify the chunk
/// kind via its extension.
///
/// Workspace-scanned files get their edges from
/// `build_dependency_graph_from_workspace`, but a disk-fallback target was never
/// passed to `update_file`. Without this, `cached_neighborhood_subgraph(uri, …)`
/// returns an empty neighborhood, so a chunk `source("R/util.R")` wouldn't
/// resolve — producing false undefined-variable positives and losing
/// missing-file context. This mirrors `backend`'s did_open: extract masked
/// metadata for the path, enrich it with the inherited working directory,
/// resolve `system.file()` entries, finalize static `tar_source()` requests,
/// install one coherent open-document record (text + metadata + artifacts),
/// pre-collect parent content for any backward directives, then update the
/// graph. The masked extraction reads chunk-body `source()`/`library()` calls
/// only (never prose).
///
/// Single source of truth for both `run`'s report loop and the
/// `collect_diagnostics_blocking` test helper, so production and test exercise
/// identical disk-fallback behavior.
fn open_disk_fallback_target(
    state: &mut crate::state::WorldState,
    uri: &Url,
    path: &Path,
    text: &str,
) {
    // Pass an honest language id so the Document classifies the chunk kind
    // correctly: "rmd" for `.Rmd`/`.Rmarkdown`/`.qmd` (the constructor reads
    // the URI to classify it as Rmd and masks the prose), "r" otherwise.
    // `file_type_from_language_id("rmd")` is `None`, so the `FileType` still
    // falls back to R via the URI — only the chunk masking differs.
    let is_jags = is_jags_file(path);
    let is_non_r = is_jags || is_stan_file(path);
    let language_id = if is_chunk_file(path) {
        "rmd"
    } else if is_jags {
        "jags"
    } else if is_non_r {
        "stan"
    } else {
        "r"
    };
    let workspace_root = state.workspace_folders.first().cloned();
    let max_chain_depth = state.cross_file_config.max_chain_depth;
    let mut meta = if is_non_r {
        crate::cross_file::CrossFileMetadata::default()
    } else {
        crate::cross_file::extract_metadata_for_path(uri.path(), text)
    };
    if !is_non_r {
        crate::cross_file::enrich_metadata_with_inherited_wd(
            &mut meta,
            uri,
            workspace_root.as_ref(),
            |parent_uri| state.get_enriched_metadata(parent_uri),
            max_chain_depth,
        );

        // Resolve system.file() source entries into concrete paths.
        let ws = state.package_state.workspace();
        let ws_name = ws.map(|w| w.name.as_str());
        let ws_root = ws.map(|w| w.root.as_path());
        let lib_paths = state.package_library.lib_paths();
        crate::cross_file::resolve_system_file_sources(&mut meta, ws_name, ws_root, lib_paths);
        let _ = crate::cross_file::tar_source::finalize_tar_source_requests_with_exclusions(
            &mut meta,
            uri,
            workspace_root.as_ref(),
            &state.workspace_exclusions,
        );
        crate::cross_file::enrich_selective_import_resolutions(
            &mut meta,
            uri,
            workspace_root.as_ref(),
        );
    }
    state.open_document_with_language_id_and_metadata(
        uri.clone(),
        text,
        Some(1),
        Some(language_id),
        std::sync::Arc::new(meta.clone()),
        None,
    );

    // Pre-collect parent content for any backward directives (`# raven: sourced-by`
    // with `match=`/inference call sites) before the mutable `update_file`
    // borrow, mirroring did_open. Forward `source()` edges — the only kind chunk
    // targets normally have — don't consult this closure, so it's empty in the
    // common case.
    let backward_path_ctx =
        crate::cross_file::path_resolve::PathContext::new(uri, workspace_root.as_ref());
    let parent_content: std::collections::HashMap<Url, String> = meta
        .sourced_by
        .iter()
        .filter_map(|d| {
            let ctx = backward_path_ctx.as_ref()?;
            let resolved = crate::cross_file::path_resolve::resolve_path(&d.path, ctx)?;
            let parent_uri = Url::from_file_path(resolved).ok()?;
            let content = state
                .documents
                .get(&parent_uri)
                .map(|doc| doc.text())
                .or_else(|| state.cross_file_file_cache.get(&parent_uri))?;
            Some((parent_uri, content))
        })
        .collect();
    let graph_meta = state.metadata_for_dependency_graph(uri, &meta, workspace_root.as_ref());
    state.cross_file_graph.update_file(
        uri,
        graph_meta.as_ref(),
        workspace_root.as_ref(),
        |parent_uri| parent_content.get(parent_uri).cloned(),
    );
}

#[derive(Debug, Default)]
struct CliContextualProviderPlan {
    extras: std::collections::HashSet<Url>,
    divergence: bool,
}

/// Materialize and select context-sensitive tar providers for one CLI target.
///
/// Workspace members reuse the consolidated index. Providers outside the scan
/// are parsed, enriched, finalized, and installed into that same index, then
/// the pure traversal is repeated so their own finalized forward closure can
/// be discovered. The returned extras supplement only snapshot content
/// collection; no occurrence node or alternate graph edge is created.
fn prepare_cli_contextual_providers(
    state: &mut crate::state::WorldState,
    uri: &Url,
) -> CliContextualProviderPlan {
    let workspace_root = state.workspace_folders.first().cloned();
    let max_depth = state.cross_file_config.max_forward_depth;
    let max_visited = state.cross_file_config.max_transitive_dependents_visited;

    let mut collected = crate::cross_file::tar_source::ContextualTarProviders::default();
    for _ in 0..max_depth.max(1) {
        let Some(root_metadata) = state.get_enriched_metadata(uri) else {
            break;
        };
        collected = crate::cross_file::tar_source::collect_contextual_tar_providers(
            uri,
            &root_metadata,
            workspace_root.as_ref(),
            max_depth,
            max_visited,
            &|provider_uri| state.get_enriched_metadata(provider_uri),
            &|parent_uri, source| {
                let target = state
                    .cross_file_graph
                    .get_dependencies(parent_uri)
                    .into_iter()
                    .find(|edge| {
                        edge.call_site_line == Some(source.line)
                            && edge.call_site_column == Some(source.column)
                            && edge.tar_source_ordinal == source.tar_source_ordinal
                            && edge.source_batch_kind == source.source_batch_kind
                    })
                    .map(|edge| edge.to.clone());
                crate::cross_file::tar_source::GraphPrefixEdgeLookup::Known(target)
            },
        );
        let mut installed_any = false;
        let mut attempted = std::collections::HashSet::new();
        for execution in &collected.executions {
            if state.get_enriched_metadata(&execution.uri).is_some()
                || !attempted.insert(execution.uri.clone())
            {
                continue;
            }
            installed_any |=
                materialize_cli_contextual_provider(state, execution, workspace_root.as_ref());
        }
        if !installed_any {
            break;
        }
    }

    let neighborhood: std::collections::HashSet<Url> = state
        .cross_file_graph
        .cached_neighborhood_subgraph(
            uri,
            state.cross_file_config.max_chain_depth,
            state.cross_file_config.max_transitive_dependents_visited,
        )
        .neighborhood
        .iter()
        .cloned()
        .collect();
    CliContextualProviderPlan {
        extras: collected
            .providers
            .into_iter()
            .filter(|provider| !neighborhood.contains(provider))
            .collect(),
        divergence: collected.divergence,
    }
}

fn materialize_cli_contextual_provider(
    state: &mut crate::state::WorldState,
    execution: &crate::cross_file::tar_source::ContextualProviderExecution,
    workspace_root: Option<&Url>,
) -> bool {
    if state.is_project_excluded_uri(&execution.uri) {
        return false;
    }
    let Ok(path) = execution.uri.to_file_path() else {
        return false;
    };
    let Ok(content) = crate::state::read_source(&path) else {
        return false;
    };
    let Ok(fs_metadata) = std::fs::metadata(&path) else {
        return false;
    };
    let document = crate::state::Document::new_with_uri(&content, None, &execution.uri);
    let is_r = document.file_type == crate::file_type::FileType::R;
    let mut metadata = document.cross_file_metadata();
    if is_r {
        if metadata.working_directory.is_none()
            && metadata.inherited_working_directory.is_none()
            && let Some(inherited) = execution.context.inherited_working_directory.as_ref()
        {
            metadata.inherited_working_directory = Some(inherited.to_string_lossy().into_owned());
        }
        if metadata.working_directory.is_none() && metadata.inherited_working_directory.is_none() {
            crate::cross_file::enrich_metadata_with_inherited_wd(
                &mut metadata,
                &execution.uri,
                workspace_root,
                |candidate| state.get_enriched_metadata(candidate),
                state.cross_file_config.max_chain_depth,
            );
        }
        let workspace = state.package_state.workspace();
        crate::cross_file::resolve_system_file_sources(
            &mut metadata,
            workspace.map(|value| value.name.as_str()),
            workspace.map(|value| value.root.as_path()),
            state.package_library.lib_paths(),
        );
        let _ = crate::cross_file::tar_source::finalize_tar_source_requests_with_exclusions(
            &mut metadata,
            &execution.uri,
            workspace_root,
            &state.workspace_exclusions,
        );
        crate::cross_file::enrich_selective_import_resolutions(
            &mut metadata,
            &execution.uri,
            workspace_root,
        );
    }
    let artifacts = std::sync::Arc::new(document.cross_file_artifacts(&execution.uri, &metadata));
    let snapshot =
        crate::cross_file::file_cache::FileSnapshot::with_content_hash(&fs_metadata, &content);
    let metadata = std::sync::Arc::new(metadata);
    let entry = crate::workspace_index::IndexEntry {
        contents: ropey::Rope::from_str(&content),
        tree: document.tree.clone(),
        loaded_packages: document.loaded_packages.clone(),
        data_packages: document.data_packages.clone(),
        snapshot: snapshot.clone(),
        metadata: metadata.clone(),
        artifacts,
        indexed_at_version: state.workspace_index.version(),
    };
    state
        .cross_file_file_cache
        .insert(execution.uri.clone(), snapshot, content);
    let (_, evicted) = state.workspace_index.install_complete_with_eviction(
        execution.uri.clone(),
        entry,
        crate::workspace_index::ClosedProvenance::Dynamic,
    );
    state.refresh_tar_source_watch_parents(std::iter::once(execution.uri.clone()).chain(evicted));
    let graph_metadata =
        state.metadata_for_dependency_graph(&execution.uri, &metadata, workspace_root);
    state.cross_file_graph.update_file(
        &execution.uri,
        graph_metadata.as_ref(),
        workspace_root,
        |_| None,
    );
    true
}

pub async fn run(args: CheckArgs) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("raven check: cannot read current directory: {e}");
            return EXIT_OPERATOR_ERROR;
        }
    };
    run_with_cwd(args, &cwd).await
}

async fn run_with_cwd(args: CheckArgs, cwd: &Path) -> i32 {
    // Workspace root: --workspace (resolved against CWD if relative), else CWD.
    // Canonicalize so `Url::from_file_path` gets an absolute path and the
    // relative paths in output are stable.
    let abs_workspace = match args.workspace {
        Some(ref p) if p.is_absolute() => p.clone(),
        Some(ref p) => cwd.join(p),
        None => cwd.to_path_buf(),
    };
    let root = std::fs::canonicalize(&abs_workspace).unwrap_or(abs_workspace);

    let Ok(workspace_url) = Url::from_file_path(&root) else {
        eprintln!("raven check: invalid workspace path: {}", root.display());
        return EXIT_OPERATOR_ERROR;
    };

    // Build the indexed WorldState (config + workspace scan + package-mode).
    let mut state = match build_indexed_state(
        &root,
        &workspace_url,
        args.no_config,
        args.config_path.as_deref(),
        cwd,
    ) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // CI default: suppress generic missing-package diagnostics because CI
    // deliberately omits installation (spec §10.1). Actionable subtypes such
    // as "available outside this active renv project" remain enabled.
    // `--report-uninstalled` opts the generic diagnostic back in.
    if !args.report_uninstalled {
        state.cross_file_config.packages_report_generic_uninstalled = false;
    }

    // Auto-detect R for installed-package / base-symbol awareness. Any failure
    // (R absent, init error, no library paths) degrades gracefully and prints
    // its own one-line note to stderr. The returned verdict feeds the
    // missing-export-metadata warning below; the returned load notes
    // (present-but-unusable package DB) ride the shared footer.
    let (shipped_db_load, db_load_notes) = maybe_init_r(&mut state, &root).await;

    // Resolve system.file() sources now that both package state AND library
    // paths are available (maybe_init_r populates lib_paths from R discovery
    // and additionalLibraryPaths).
    state.resolve_system_file_in_workspace_cli_compat(None);

    // R fallback for sysdata: when the AST scan found nothing AND
    // R/sysdata.rda exists, load it via the R subprocess (see
    // `maybe_load_sysdata_fallback`).
    maybe_load_sysdata_fallback(&mut state).await;

    // Resolve which files to report diagnostics for. A named path that does not
    // exist is an operator error (exit 2), matching `raven lint`.
    let mut operator_error = false;
    let targets = collect_report_targets_with_exclusions(
        &args.paths,
        &root,
        &state.workspace_exclusions,
        &mut operator_error,
    );

    // Warm the package-export cache before computing diagnostics, matching the
    // editor's post-scan prefetch (see [`prefetch_reported_packages`]).
    prefetch_reported_packages(&state, &targets).await;

    let (all_diags, reported_loaded_packages, collect_operator_error) =
        collect_target_diagnostics(&mut state, &targets).await;
    operator_error |= collect_operator_error;

    // Issue #483 (WI2b): report standalone-scope cache effectiveness at trace
    // level — proves cross-snapshot reuse on hub workspaces (and is a cheap
    // diagnosis hook). No-op unless `RUST_LOG=raven::cli=trace`.
    log::trace!(
        "standalone scope cache: {} hits, {} misses",
        state.standalone_scope_cache.hits(),
        state.standalone_scope_cache.misses()
    );

    let any_above_threshold = all_diags
        .iter()
        .any(|(_, d)| SeverityLevel::from_diag(d) > args.max_severity);

    let missing_export_metadata_packages =
        if should_check_missing_export_metadata(&state, &all_diags) {
            collect_missing_export_metadata_packages(&state, &reported_loaded_packages).await
        } else {
            Vec::new()
        };

    let use_color = resolve_color_from_env(args.color);
    render(args.format, &all_diags, &root, args.quiet, use_color);

    // Post-render context notes that annotate the diagnostics above:
    //   0. package-DB load notes (a present-but-unusable `names.db` /
    //      `.raven/packages.json`), surfaced from `maybe_init_r`. They lead the
    //      footer so the missing-export warning's "failed to load" wording reads
    //      naturally after the specific load error.
    //   1. the missing-export-metadata warning (some attached packages' symbols
    //      couldn't be loaded, so undefined-variable findings may be inaccurate);
    //   2. the cross-file traversal-budget note (issue #473) — when a budget is
    //      hit the resolver silently stops following `source()` edges, so some
    //      "Undefined variable" warnings above may be false positives. The owned
    //      `state.cross_file_graph` accumulates these counters across every
    //      target's diagnostic pass, so reading them after the loop reflects the
    //      whole run. The editor surfaces the same via a throttled
    //      `window/showMessage`; `raven check` had no equivalent.
    //
    // All are emitted together so they share a stream and stay grouped:
    // `footer_stream` keeps them on the diagnostics' own stream (stdout for
    // `text`, where a merged terminal/CI consumer would otherwise interleave
    // them with the findings; stderr for json/sarif, which reserve stdout for
    // the machine document). See `footer_stream`.
    let mut footer = db_load_notes;
    if !missing_export_metadata_packages.is_empty() {
        footer.push(format_missing_export_metadata_warning(
            &missing_export_metadata_packages,
            shipped_db_load,
        ));
    }
    if let Some(note) = format_traversal_budget_note(&state) {
        footer.push(note);
    }
    // The deduplicated NSE-discoverability footer is `text`-only. The hint is
    // never inline on a finding (json/sarif/editor all show the bare
    // "`x` is not defined"); it rides `Diagnostic.data` and surfaces only here,
    // once, aggregated and deduplicated. See `format_nse_hint_footer`.
    if args.format == OutputFormat::Text
        && let Some(note) = format_nse_hint_footer(&all_diags)
    {
        footer.push(note);
    }
    if !footer.is_empty() {
        // The notes carry no header and no per-line prefix: this is `raven
        // check`'s own contiguous output, so labeling it `raven check:` mid-stream
        // is redundant self-reference (cf. ESLint, which prints its trailing
        // summary unlabeled). A leading blank line separates the block from the
        // summary line `render` just wrote, and notes are joined by a blank line
        // so stacked notes read as distinct paragraphs rather than running
        // together. (The standalone degraded-environment startup note on stderr
        // keeps a `raven check: ` prefix — there it is a lone line that can
        // interleave with other programs' output, where the prefix earns its
        // place; see `degraded_env_note`.)
        let body = format!("\n{}", footer.join("\n\n"));
        match footer_stream(args.format) {
            FooterStream::Stdout => {
                use std::io::Write as _;
                // Lock + flush so the footer lands after the diagnostics already
                // written by `render` and is fully drained before process exit.
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{body}");
                let _ = out.flush();
            }
            FooterStream::Stderr => eprintln!("{body}"),
        }
    }

    // Operator error takes priority over a threshold breach: a half-read run
    // shouldn't masquerade as a clean (or merely failing) lint result.
    if operator_error {
        EXIT_OPERATOR_ERROR
    } else if any_above_threshold {
        EXIT_LINT_FAILED
    } else {
        EXIT_OK
    }
}

/// Build a fully-indexed `WorldState`: load project config, scan the workspace,
/// and derive package-mode scope. The R package library is initialized
/// separately (see [`maybe_init_r`]) since it depends on an external process.
///
/// This is the synchronous, single-owner counterpart to the LSP server's
/// startup (`backend::initialized`, "Task B"). The two intentionally differ
/// only in *wiring* — the server is async, takes write locks, and records perf;
/// the CLI owns `state` exclusively — while every piece of *logic* that could
/// drift is single-sourced through shared seams: config discovery+load
/// ([`crate::config_file::discover_and_load`]), the workspace scan
/// ([`crate::state::scan_workspace`] + [`crate::state::WorldState::apply_workspace_index`]),
/// package-input seeding ([`crate::backend::initialize_package_inputs_from_state_with_exclusions`]),
/// and the R package library ([`crate::package_library::build_package_library`]).
/// Keep new startup logic in those seams, not duplicated here.
fn build_indexed_state(
    root: &Path,
    workspace_url: &Url,
    no_config: bool,
    config_path: Option<&Path>,
    explicit_config_base: &Path,
) -> Result<crate::state::WorldState, i32> {
    let (project_settings, project_config_path) =
        resolve_project_config(no_config, config_path, root, explicit_config_base)?;

    let mut state = crate::state::WorldState::new();
    state.workspace_folders = vec![workspace_url.clone()];
    state.raw_project_settings = project_settings;
    state.project_config_path = project_config_path;
    // `recompute_parsed_configs` is the only writer of the parsed config fields.
    // It reads `project_config_path` to detect a discovered `.lintr` and gate
    // its auto-enable (which, for the CLI's empty client layer, defaults on).
    crate::config_file::recompute_parsed_configs(&mut state);

    // Index the workspace exactly as the LSP server does on startup. This is
    // rayon-parallel internally; there's no lock contention here since the CLI
    // owns `state` exclusively.
    let max_chain_depth = state.cross_file_config.max_chain_depth;
    let entries = crate::state::scan_workspace_with_exclusions(
        std::slice::from_ref(workspace_url),
        max_chain_depth,
        &state.workspace_exclusions,
    );
    state.apply_workspace_index(entries);

    // Derive package-mode scope (so `R/*.R` files in an R package see each
    // other's top-level definitions without explicit `source()`). This is
    // independent of the R subprocess — it's derived from the workspace files,
    // DESCRIPTION, and NAMESPACE — but MUST run after `apply_workspace_index`,
    // which resets package state.
    //
    // The disk seed is empty: `initialize_package_inputs_from_state_with_exclusions` hydrates
    // every package R file from the workspace index we just applied, so reading
    // them from disk here would only be overwritten. This mirrors the LSP's
    // with-scan startup path, which seeds package inputs from disk only on the
    // no-scan branch (see `backend.rs`).
    let desc_text: Option<std::sync::Arc<str>> = std::fs::read_to_string(root.join("DESCRIPTION"))
        .ok()
        .map(|t| t.into());
    let ns_text: Option<std::sync::Arc<str>> = std::fs::read_to_string(root.join("NAMESPACE"))
        .ok()
        .map(|t| t.into());
    let workspace_exclusions = state.workspace_exclusions.clone();
    crate::backend::initialize_package_inputs_from_state_with_exclusions(
        &mut state,
        root.to_path_buf(),
        desc_text,
        ns_text,
        Default::default(),
        // `raven check` is a single-pass batch with no concurrent writers, so
        // both `None`s let the helper scan `.Rprofile` and the testthat
        // preamble inline (no off-lock precompute needed for either). See
        // `initialize_package_inputs_from_state_with_exclusions`.
        None,
        None,
        &workspace_exclusions,
    );

    Ok(state)
}

/// Discover and load the project config at or above `search_start` (the search
/// itself is done by `find_config`). Explicit relative `--config` paths resolve
/// from `explicit_config_base`, i.e. the command invocation CWD, not the selected
/// workspace root. Returns `(settings, config_path)` to wire into the
/// `WorldState`. Prints warnings to stderr; returns `Err(EXIT_OPERATOR_ERROR)`
/// when a config that exists cannot be loaded.
fn resolve_project_config(
    no_config: bool,
    config_path: Option<&Path>,
    search_start: &Path,
    explicit_config_base: &Path,
) -> Result<(Option<serde_json::Value>, Option<PathBuf>), i32> {
    resolve_project_config_with_options(
        no_config,
        config_path,
        search_start,
        explicit_config_base,
        &crate::config_file::DiscoveryOptions::default(),
    )
}

fn resolve_project_config_with_options(
    no_config: bool,
    config_path: Option<&Path>,
    search_start: &Path,
    explicit_config_base: &Path,
    discovery_options: &crate::config_file::DiscoveryOptions,
) -> Result<(Option<serde_json::Value>, Option<PathBuf>), i32> {
    if no_config {
        return Ok((None, None));
    }
    // Every loader yields `settings` + `warnings`; emit the warnings and tag the
    // settings with the config path they came from.
    let loaded = |warnings: Vec<String>, settings: serde_json::Value, path: PathBuf| {
        for w in warnings {
            eprintln!("{w}");
        }
        Ok((Some(settings), Some(path)))
    };
    if let Some(explicit) = config_path {
        return match crate::config_file::load_explicit_config_from_base(
            explicit_config_base,
            explicit,
        ) {
            Some((explicit_abs, l)) => loaded(l.warnings, l.settings, explicit_abs),
            None => {
                eprintln!(
                    "raven check: failed to load --config {}",
                    explicit.display()
                );
                Err(EXIT_OPERATOR_ERROR)
            }
        };
    }
    // Discovery (raven.toml beats .lintr) and loading are shared with the LSP
    // server via `config_file::discover_and_load`, so the CLI and editor can't
    // drift on discovery precedence or which loader reads `.lintr`.
    match crate::config_file::discover_and_load_with_options(search_start, discovery_options) {
        crate::config_file::DiscoveredLoad::Loaded {
            path,
            settings,
            warnings,
        } => loaded(warnings, settings, path),
        crate::config_file::DiscoveredLoad::LoadFailed { path } => {
            eprintln!("raven check: failed to load {}", path.display());
            Err(EXIT_OPERATOR_ERROR)
        }
        crate::config_file::DiscoveredLoad::None => Ok((None, None)),
    }
}

/// Auto-detect R and store the resulting package library on `state`, so
/// installed-package exports and base R symbols are available.
///
/// The shared construction and classification rules — the `packages.enabled`
/// gate *before* any R discovery, `packages.rPath` selection, applying
/// `packages.additionalLibraryPaths` *after* discovery, and the readiness
/// predicate — all live in [`crate::package_library::build_package_library`];
/// see its doc comment for that contract. Routing through it is what keeps this
/// CLI path and the editor's startup paths from drifting.
///
/// This function owns only the *caller policy*: it always installs the returned
/// library, then uses `PackageLibraryOutcome::consumer_ready` for the diagnostic
/// readiness gate. The three R-related degradations print a one-line note to
/// stderr so CI shows what was missing — but only when no offline package data
/// loaded (`!has_providers()`): a degraded R build whose coverage is served by
/// an updated `names.db` or frozen `.raven/packages.json` is not actually
/// degraded for package symbols, so warning (and recommending `raven packages
/// update`, which already ran) would be a false alarm. `Disabled` (the user
/// turned package awareness off in `raven.toml`) is silent, matching the editor.
///
/// Returns `(shipped_db_load, load_notes)`:
/// - the build's [`crate::package_library::ShippedDbLoad`] verdict, so the caller can tailor the
///   missing-export-metadata warning (absent / loaded / present-but-failed)
///   without re-stat-ing disk — the build already knows whether the shipped
///   `names.db` actually loaded, which a bare `Path::exists()` cannot tell;
/// - the present-but-unusable package-DB load notes (unprefixed; the caller's
///   footer prints the notes blank-line-separated with no header). The caller folds these into the shared footer rather
///   than this function printing them, so they ride the diagnostics' own stream
///   (stdout for `text`) instead of being reorderable against the findings on
///   stderr. See `footer_stream`.
async fn maybe_init_r(
    state: &mut crate::state::WorldState,
    root: &Path,
) -> (crate::package_library::ShippedDbLoad, Vec<String>) {
    // Snapshot config into locals before the call so the later `state`
    // mutation doesn't conflict with the borrow.
    let r_path = state.cross_file_config.packages_r_path.clone();
    let additional = state
        .cross_file_config
        .packages_additional_library_paths
        .clone();
    let enabled = state.cross_file_config.packages_enabled;

    let outcome = crate::package_library::build_package_library(
        r_path,
        &additional,
        Some(root.to_path_buf()),
        enabled,
    )
    .await;

    // Present-but-unusable package-DB notes (e.g. a `.raven/packages.json` from a
    // newer Raven, or a corrupt/incompatible `names.db`) are build-time events
    // carried on the outcome. Return them to the caller rather than printing here
    // so they ride the shared footer on the diagnostics' own stream (stdout for
    // `text`); printing to stderr inline would let a merged CI consumer reorder
    // them relative to the findings. Extract before the status match below
    // partially moves `outcome`.
    // No `raven check: ` prefix: these ride the caller's shared footer, which
    // prints its notes blank-line-separated with no header.
    let load_notes: Vec<String> = outcome.load_notes.clone();

    // Always install the returned library: on a non-`Ready` status it may still
    // carry Tier 2/3 providers or bundled base exports, which are the whole
    // point of CI resolution without R. Dropping it here would send `raven
    // check` back to an empty library and lose the offline path.
    state.package_library_ready = outcome.consumer_ready();
    let status = outcome.status;
    let shipped_db_load = outcome.shipped_db_load;
    state.package_library = outcome.library;
    // A freshly built `PackageLibrary` starts with a `None` local-dev overlay, so
    // replacing `state.package_library` here drops the overlay that
    // `build_indexed_state`'s `apply_package_event(Initial)` installed. Rebuild it
    // from the (already-derived) package contribution, exactly as the LSP's
    // library-replacing paths do (see `refresh_local_dev_overlay`'s doc). Without
    // this, a `raven check` on a package whose in-root script calls
    // `devtools::load_all()` loses sentinel resolution and reports every package
    // internal as an undefined variable. Runs before `maybe_load_sysdata_fallback`,
    // which may refresh the overlay again after adding R-loaded sysdata names.
    state.refresh_local_dev_overlay();
    // The degraded-environment startup note's decision is factored into
    // `degraded_env_note` (pure, hence unit-testable without an R environment —
    // `RNotFound` is unreachable end-to-end on any machine with R installed,
    // because `get_fallback_lib_paths()` finds the framework library and the
    // build classifies `Ready`). `has_providers()` is the coverage gate: offline
    // Tier 2/3 data resolves package symbols without R, so the note is suppressed.
    if let Some(note) = degraded_env_note(&status, state.package_library.has_providers()) {
        eprintln!("raven check: {note}");
    }
    (shipped_db_load, load_notes)
}

/// Shared tail of the degraded-environment startup notes: the consequence
/// (coverage limited to base R, still covered by the embedded database) and the
/// remedy. Identical across all three causes because the limitation and fix
/// don't depend on *why* the live R library was unavailable.
const DEGRADED_ENV_NOTE_TAIL: &str = "package symbol resolution is limited to base R (covered by the embedded database). Run `raven packages update` for broad CRAN/Bioconductor coverage";

/// The degraded-environment startup note for a package-library build, or `None`
/// when no note should print. (The caller prefixes `raven check: ` and writes it
/// to stderr.)
///
/// `None` when the build is `Ready` (live R) or `Disabled` (package awareness
/// off) — neither is degraded — or when `has_offline_packages`: an updated
/// `names.db` or frozen `.raven/packages.json` resolves package symbols without
/// R, so the warning would be a false alarm (and would tell a CI that already
/// ran `raven packages update` to run it again). The gate is `has_providers()`,
/// NOT `consumer_ready()`/`base_exports()`: base-only coverage is exactly the
/// "limited to base R" case the note exists to flag. `build_package_library`
/// does not wire a zero-package provider (an empty `names.db` /
/// `.raven/packages.json`), so `has_providers()` means real package coverage
/// exists — not merely that an offline file was present.
fn degraded_env_note(
    status: &crate::package_library::PackageLibraryStatus,
    has_offline_packages: bool,
) -> Option<String> {
    use crate::package_library::PackageLibraryStatus::*;
    match status {
        Ready | Disabled => None,
        _ if has_offline_packages => None,
        RNotFound => Some(format!("R not found on PATH; {DEGRADED_ENV_NOTE_TAIL}")),
        InitFailed(e) => Some(format!(
            "R found but its package library failed to initialize ({e}); {DEGRADED_ENV_NOTE_TAIL}"
        )),
        NoLibraryPaths => Some(format!(
            "R found but no library paths were discovered; {DEGRADED_ENV_NOTE_TAIL}"
        )),
    }
}

/// `raven check`'s counterpart of the LSP startup's sysdata R fallback (the
/// "R fallback for sysdata" block in `backend.rs`): when the AST scan of
/// `data-raw/` found nothing but a binary `R/sysdata.rda` exists on disk
/// (e.g. r-lib/cli commits the `.rda` with no generating script), load it via
/// the package library's R subprocess so the package's own `R/` code can
/// reference its internal data objects without false undefined-variable
/// findings. The trigger predicate is shared
/// ([`crate::backend::sysdata_r_fallback_needed`]) so the two paths can't
/// drift. The names feed only package-mode scope (`contrib.sysdata_symbols`);
/// a user script that attaches the package still flags these objects —
/// `library(cli); emojis` remains a real R error.
///
/// Must run after [`maybe_init_r`] (needs the library's R subprocess) and
/// before diagnostics. Fail-soft: no R, no `.rda`, or a load failure leaves
/// the AST-scan result in place.
async fn maybe_load_sysdata_fallback(state: &mut crate::state::WorldState) {
    if !crate::backend::sysdata_r_fallback_needed(state) {
        return;
    }
    let Some(root) = state.package_inputs.workspace_root.clone() else {
        return;
    };
    let names = match state.package_library.r_subprocess() {
        Some(r) => crate::package_state::sysdata::load_sysdata_via_r(r, &root).await,
        None => return,
    };
    if !names.is_empty() {
        state.package_inputs.sysdata_names = names;
        state.apply_package_event(&crate::package_state::PackageInputDelta::DataDirChanged);
    }
}

/// Warm the package-export cache for the packages the reported files attach,
/// matching the editor's post-scan prefetch
/// ([`crate::backend::prefetch_packages_for_open_documents`]).
///
/// The undefined-variable check is synchronous and treats an installed-but-
/// uncached package as "pending", suppressing bare calls that could resolve to
/// it (`handlers.rs`). Without this warm-up `raven check` would under-report
/// undefined symbols from attached packages relative to the editor whenever R
/// (or a configured library path) makes the package installed-but-uncached.
///
/// Covers the directly-attached packages (`library()` / `require()`) of each
/// reported file. R-source targets read their already-parsed `loaded_packages`
/// from the workspace index (free). Chunk-bearing targets (`.Rmd`/
/// `.Rmarkdown`/`.qmd`) are never in the R-only scan, so they're read from disk
/// and masked here so a chunk `library()` call warms its package's exports too
/// — without this the undefined-variable check would conservatively suppress
/// bare calls to an installed-but-uncached package attached only inside a chunk,
/// under-reporting relative to the editor.
///
/// In package mode, also covers NAMESPACE whole-package `import(pkg)`
/// directives (`scope_contribution.full_imports`) and packages attached by
/// testthat preamble files (`scope_contribution.test_helper_attached_packages`,
/// issue #432). For both, the undefined-variable
/// check resolves call- and value-position uses of the package's exports via
/// `is_package_export`, which needs the cache warm; without this
/// the "pending" heuristic suppresses only call-position uses, so
/// value-position references (default args, bare identifiers) would emit
/// false "Undefined variable" diagnostics — an asymmetry the editor avoids.
/// (The helper-attach union is flat; the source-order gate that governs
/// visibility is irrelevant to warming.)
///
/// Cross-file *inherited* packages (attached in a `source()`d file) and
/// packages of non-chunk targets the scan did not index are not prefetched
/// here, so calls relying on those stay conservatively suppressed — a narrower
/// gap than before, noted in `docs/cli.md`. No-op when the library isn't ready
/// (e.g. R absent with no configured library paths).
async fn prefetch_reported_packages(state: &crate::state::WorldState, targets: &[PathBuf]) {
    if !state.package_library_ready {
        return;
    }
    let packages: Vec<String> = reported_packages_to_warm(state, targets)
        .into_iter()
        .filter(|p| crate::r_subprocess::is_valid_package_name(p))
        .collect();
    // `prefetch_packages` is a no-op on an empty slice, so no length guard here.
    state.package_library.prefetch_packages(&packages).await;
}

/// Pure (R-free) computation of the package set [`prefetch_reported_packages`]
/// warms, before the validity filter and the async cache prefetch. Extracted so
/// the warming wiring is unit-testable without an R subprocess. Reads target
/// files from disk for chunk files (best-effort), but performs no R calls.
fn reported_packages_to_warm(
    state: &crate::state::WorldState,
    targets: &[PathBuf],
) -> std::collections::HashSet<String> {
    let mut packages: std::collections::HashSet<String> = std::collections::HashSet::new();
    let workspace_root = state.workspace_folders.first();
    let mut selective_roots = targets
        .iter()
        .filter_map(|path| Url::from_file_path(path).ok())
        .collect::<Vec<_>>();
    // Package mode: NAMESPACE `import(pkg)` puts every export of `pkg` in
    // scope for the package's own R files, so warm those exports too.
    packages.extend(
        state
            .package_state
            .scope_contribution()
            .full_imports
            .iter()
            .cloned(),
    );
    packages.extend(
        state
            .package_state
            .scope_contribution()
            .rprofile_attached_packages
            .iter()
            .cloned(),
    );
    // Package mode: packages attached by testthat preamble files
    // (`helper*.R`/`setup*.R`) via `library()`/`require()` propagate to sibling
    // test files (issue #432), so warm their exports too — the undefined-
    // variable check resolves them via `is_package_export` reading the
    // (preamble-injected) `inherited_packages`, which needs the cache warm.
    // Warming is a union, so the source-order gate that governs *visibility*
    // (see `append_package_contribution`) is irrelevant here.
    for pkgs in state
        .package_state
        .scope_contribution()
        .test_helper_attached_packages
        .values()
    {
        packages.extend(pkgs.iter().cloned());
    }
    for path in targets {
        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };
        if let Some(entry) = state.workspace_index.get(&uri) {
            packages.extend(entry.loaded_packages.iter().cloned());
            // Conditional bare `p_load()` targets are deliberately absent
            // from `IndexEntry.loaded_packages`: flat extraction cannot prove
            // their attachment prerequisite. Warming metadata is permissive
            // and cannot attach a package, so include every validated
            // metadata-derived target here. Semantic scope still decides
            // whether its exports are visible, while an active target is no
            // longer diagnosed before its installed export metadata is ready.
            packages.extend(
                entry
                    .metadata
                    .library_calls
                    .iter()
                    .filter(|call| call.requires_attached.is_some())
                    .map(|call| call.package.clone()),
            );
            // Issue #429: warm packages named in `data(..., package = "pkg")`
            // so the diagnostics-time `data()` alias expansion can resolve the
            // dataset object names. Unlike `library()` these are not attached,
            // but their `data/` enumeration must be cached.
            packages.extend(entry.data_packages.iter().cloned());
            // Issue #662: static `box::use(package)` imports need the same
            // canonical package metadata warm-up as the LSP. They do not attach
            // the package; warming only makes the selective export boundary
            // available to diagnostics and scope resolution.
            packages.extend(entry.metadata.box_imports.iter().filter_map(
                |import| match &import.spec {
                    crate::box_use::BoxSpec::Package(package) => Some(package.clone()),
                    crate::box_use::BoxSpec::LocalModule { .. }
                    | crate::box_use::BoxSpec::Unsupported(_) => None,
                },
            ));
            packages.extend(
                entry
                    .metadata
                    .import_calls
                    .iter()
                    .filter_map(crate::import_pkg::ImportCall::package_name)
                    .map(str::to_string),
            );
        } else if is_chunk_file(path) {
            // Chunk files are outside the R-only scan, so they have no index
            // entry. Read + construct a throwaway Document best-effort so its
            // masked `loaded_packages` (chunk-body `library()`/`require()` calls
            // only, never prose) drive the warm-up exactly as an indexed R
            // file's would. An unreadable / mis-encoded file just contributes no
            // packages — its read failure surfaces as a finding in the report
            // loop.
            if let Ok(text) = crate::state::read_source(path) {
                let doc =
                    crate::state::Document::new_with_language_id(&text, Some(1), &uri, Some("rmd"));
                packages.extend(doc.loaded_packages.iter().cloned());
                packages.extend(doc.data_packages.iter().cloned());
                let mut metadata = crate::cross_file::extract_metadata(&doc.analysis_text());
                crate::cross_file::enrich_selective_import_resolutions(
                    &mut metadata,
                    &uri,
                    workspace_root,
                );
                selective_roots.extend(metadata.selective_import_requests(&uri).filter_map(
                    |request| match request.source {
                        crate::selective_import::ImportSource::LocalModule(module) => {
                            Some(module.uri)
                        }
                        crate::selective_import::ImportSource::Package(_) => None,
                    },
                ));
                packages.extend(
                    metadata
                        .library_calls
                        .iter()
                        .filter(|call| call.requires_attached.is_some())
                        .map(|call| call.package.clone()),
                );
                packages.extend(metadata.box_imports.iter().filter_map(
                    |import| match &import.spec {
                        crate::box_use::BoxSpec::Package(package) => Some(package.clone()),
                        crate::box_use::BoxSpec::LocalModule { .. }
                        | crate::box_use::BoxSpec::Unsupported(_) => None,
                    },
                ));
                packages.extend(
                    metadata
                        .import_calls
                        .iter()
                        .filter_map(crate::import_pkg::ImportCall::package_name)
                        .map(str::to_string),
                );
            }
        }
    }

    // Report targets may import package-using local modules without naming those
    // packages themselves. Traverse only typed selective-module edges: ordinary
    // source() children keep their existing, separately documented warming rules.
    for module_uri in state
        .cross_file_graph
        .selective_module_closure(selective_roots)
    {
        let Some(metadata) = state.get_enriched_metadata(&module_uri) else {
            continue;
        };
        packages.extend(
            metadata
                .referenced_packages()
                .filter(|package| !crate::package_library::is_load_all_sentinel(package))
                .map(str::to_string),
        );
    }

    packages
}

/// Graph-aware conditional package targets active at their call sites in one
/// indexed file. Unlike the permissive warm set, this is suitable for
/// user-facing missing-metadata reporting.
fn active_conditional_packages_for_uri(state: &crate::state::WorldState, uri: &Url) -> Vec<String> {
    let metadata = state
        .documents
        .get_record(uri)
        .map(|record| record.metadata().clone())
        .or_else(|| state.workspace_index.get_metadata(uri));
    let Some(metadata) = metadata else {
        return Vec::new();
    };
    let get_artifacts = |target: &Url| {
        state
            .documents
            .get_record(target)
            .map(|record| record.artifacts().clone())
            .or_else(|| {
                state
                    .workspace_index
                    .get(target)
                    .map(|item| item.artifacts.clone())
            })
    };
    let get_metadata = |target: &Url| {
        state
            .documents
            .get_record(target)
            .map(|record| record.metadata().clone())
            .or_else(|| state.workspace_index.get_metadata(target))
    };
    let workspace_root = deepest_workspace_root(&state.workspace_folders, uri);
    let mut active = Vec::new();
    let mut attachment_cache = std::collections::HashMap::new();
    let mut prefix_cache = crate::cross_file::scope::ParentPrefixCache::new();
    let package_contribution = state.package_state.scope_contribution();
    let base_exports = if state.package_library_ready {
        state.package_library.base_exports().as_ref()
    } else {
        crate::handlers::empty_base_exports().as_ref()
    };
    for call in &metadata.library_calls {
        let Some(required) = call.requires_attached.as_deref() else {
            continue;
        };
        let (line, column) = crate::cross_file::source_detect::position_before_library_call(call);
        let attached_packages = attachment_cache.entry((line, column)).or_insert_with(|| {
            crate::cross_file::scope::scope_at_position_with_graph_cached(
                uri,
                line,
                column,
                &get_artifacts,
                &get_metadata,
                &state.cross_file_graph,
                workspace_root,
                state.cross_file_config.max_chain_depth,
                base_exports,
                state.cross_file_config.hoist_globals_in_functions,
                state.cross_file_config.backward_dependencies,
                &|| false,
                &mut prefix_cache,
                Some(package_contribution),
                None,
            )
            .attached_packages
        });
        if attached_packages.contains(required)
            && crate::r_subprocess::is_valid_package_name(&call.package)
        {
            active.push(call.package.clone());
        }
    }
    active
}

/// Select the most specific workspace folder containing `uri` by filesystem
/// path components, never by a raw URI-string prefix.
fn deepest_workspace_root<'a>(roots: &'a [Url], uri: &Url) -> Option<&'a Url> {
    let path = uri.to_file_path().ok()?;
    roots
        .iter()
        .filter_map(|root| root.to_file_path().ok().map(|root_path| (root, root_path)))
        .filter(|(_, root_path)| path.starts_with(root_path))
        .max_by_key(|(_, root_path)| root_path.components().count())
        .map(|(root, _)| root)
}

fn has_package_metadata_sensitive_undefined_diagnostic(
    all_diags: &[(PathBuf, Diagnostic)],
) -> bool {
    // Anchor on the stable rule code, not the (free-prose) message. The
    // `undefined-variable` code covers three variants and only the plain one
    // ("<name> is not defined") is resolvable by package export metadata. The
    // two position/ordering variants (forward reference, or a symbol sourced
    // later — the symbol exists, it is just not visible at the use site) are
    // tagged by the emitters via `Diagnostic.data`, so we exclude them on that
    // structured marker rather than parsing the message: the message prepends
    // the raw (possibly backtick-quoted) symbol name, so any substring test
    // could be spoofed by a pathological name. See the undefined-variable
    // emitters in handlers.rs and `UNDEFINED_VARIABLE_POSITION_VARIANT`.
    all_diags.iter().any(|(_, d)| {
        crate::diagnostic_code::diagnostic_has_code(
            &d.code,
            crate::diagnostic_code::UNDEFINED_VARIABLE,
        ) && !crate::diagnostic_code::is_undefined_variable_position_variant(&d.data)
    })
}

fn should_check_missing_export_metadata(
    state: &crate::state::WorldState,
    all_diags: &[(PathBuf, Diagnostic)],
) -> bool {
    state.cross_file_config.packages_enabled
        && has_package_metadata_sensitive_undefined_diagnostic(all_diags)
}

/// Caller must gate on [`should_check_missing_export_metadata`] (which already
/// requires `packages_enabled`); this only walks the reported packages.
async fn collect_missing_export_metadata_packages(
    state: &crate::state::WorldState,
    reported_loaded_packages: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut missing = Vec::new();
    for package in reported_loaded_packages
        .iter()
        .filter(|p| crate::r_subprocess::is_valid_package_name(p))
    {
        if state.package_library.export_metadata_missing(package).await {
            missing.push(package.clone());
        }
    }
    missing
}

/// Warn that some attached packages' exported symbols couldn't be loaded, so the
/// undefined-variable diagnostics above may be unreliable — then steer the user
/// to a fix. Leads with impact, then the obvious remedy (install the package),
/// then a database-specific fallback.
///
/// The fallback depends on the Tier 3 shipped database's actual load state
/// ([`crate::package_library::ShippedDbLoad`]) — a three-way signal, not a present/absent boolean. The
/// distinction matters because `p.exists()` ("a database file is on disk") does
/// NOT mean it loaded and was searched: a corrupt/unsupported file also exists.
///
/// - `Loaded` — the database loaded and was searched; the package genuinely
///   isn't in it, so it's likely private or off CRAN/Bioconductor → `freeze`.
/// - `Absent` — no database installed → `raven packages update` to download it
///   (or `freeze` a snapshot from a machine where the package is installed).
/// - `Failed` — a database file is present but corrupt/unsupported, so it was
///   never searched. `freeze` is the wrong advice here (the package may well be
///   in a working copy); steer to `raven packages update` to re-download a good
///   copy. The specific load error is printed separately as a load note.
fn format_missing_export_metadata_warning(
    packages: &[String],
    shipped_db_load: crate::package_library::ShippedDbLoad,
) -> String {
    use crate::package_library::ShippedDbLoad::*;

    let mut names = packages.to_vec();
    names.sort();
    names.dedup();
    let n = names.len();
    names.truncate(8);
    let names = names.join(", ");

    // Count-aware nouns/pronouns so a single package reads naturally.
    let (noun, obj, inst) = if n == 1 {
        ("this package", "it", "it's")
    } else {
        ("these packages", "them", "they're")
    };

    let head = format!(
        "couldn't load exported symbols for {names}.\n\
         Some \"Undefined variable\" warnings above may be inaccurate as a result.\n\
         To fix: install {noun} in your R library."
    );

    match shipped_db_load {
        Loaded => format!(
            "{head}\n\
             Raven's package symbol database doesn't provide {obj} — {inst} likely private or not on CRAN/Bioconductor.\n\
             Capture {obj} with `raven packages freeze` on a machine where {inst} installed, and commit the result."
        ),
        Absent => format!(
            "{head}\n\
             In CI without R, run `raven packages update` before `raven check` to download Raven's package symbol database,\n\
             or commit a `raven packages freeze` snapshot made on a machine where {inst} installed."
        ),
        Failed => format!(
            "{head}\n\
             Raven's package symbol database is present but failed to load (corrupt or unsupported format), so it was not searched.\n\
             Run `raven packages update` to re-download a working database."
        ),
    }
}

/// Which stream the post-render context-note footer goes to.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum FooterStream {
    Stdout,
    Stderr,
}

/// Decide which stream the post-render context-note footer goes to, given the
/// output format. Pure (no I/O) so the routing rule is unit-testable without
/// spawning the binary or an R subprocess; the caller owns the footer `Vec` and
/// does the actual writing (no copy here).
///
/// The footer notes (package-DB load note, missing-export-metadata warning,
/// traversal-budget note) annotate the diagnostics, so for the human-readable
/// `text` format they MUST share the diagnostics' stream — stdout. stdout and
/// stderr are independent OS pipes that a merged consumer (a terminal, `2>&1`,
/// or GitHub Actions — which timestamps each line at *read* time across two
/// reader threads) reorders freely, so splitting a logically-grouped report
/// across both streams interleaves the notes with the findings they describe.
/// Same stream ⇒ a single reader preserves order. For the machine formats
/// (`json`/`sarif`) stdout carries the parsed document, so the footer goes to
/// stderr where it can't corrupt it.
fn footer_stream(format: OutputFormat) -> FooterStream {
    match format {
        OutputFormat::Text => FooterStream::Stdout,
        OutputFormat::Json | OutputFormat::Sarif => FooterStream::Stderr,
    }
}

/// Build the cross-file traversal-budget note for `raven check`, or `None` if
/// no traversal was truncated (issue #473).
///
/// Two independent budgets can truncate cross-file analysis: the visited-node
/// budget (`maxTransitiveDependentsVisited`), recorded by the neighborhood walk
/// on the full graph; and the chain-depth limit (`maxChainDepth`) on the
/// bidirectional neighborhood walk (resolver-level depth exceedances also
/// surface per-site as `depth_exceeded` diagnostics). Either truncation can drop
/// `source()` edges, so dropped symbols may appear as false-positive
/// `undefined-variable` warnings. The note tells the user which budget to raise
/// so a budget-induced drop is distinguishable from a genuine undefined variable
/// in CI.
fn format_traversal_budget_note(state: &crate::state::WorldState) -> Option<String> {
    let visited_trunc = state.cross_file_graph.visited_budget_truncations();
    let depth_trunc = state.cross_file_graph.depth_truncations();
    if visited_trunc == 0 && depth_trunc == 0 {
        return None;
    }
    let mut lines = Vec::new();
    if visited_trunc > 0 {
        let max_visited = state.cross_file_config.max_transitive_dependents_visited;
        lines.push(format!(
            "a bounded cross-file neighborhood traversal was truncated \
             (maxTransitiveDependentsVisited = {max_visited}); some source() edges were \
             not followed, so some \"Undefined variable\" warnings above may be false \
             positives. Raise `crossFile.maxTransitiveDependentsVisited` in raven.toml to \
             analyze more files."
        ));
    }
    if depth_trunc > 0 {
        let max_chain_depth = state.cross_file_config.max_chain_depth;
        lines.push(format!(
            "a cross-file traversal hit the depth limit \
             (maxChainDepth = {max_chain_depth}); some deeply-nested source() edges were \
             not followed. Raise `crossFile.maxChainDepth` in raven.toml to analyze deeper \
             dependency chains."
        ));
    }
    // Blank line between the two sub-notes so they read as distinct paragraphs
    // in the footer (which carries no header and no per-note prefix).
    Some(lines.join("\n\n"))
}

/// Stable, public docs links for the NSE footer. raven has no hosted docs site
/// — docs live in the repo and `README.md` points users to GitHub — so these
/// are the canonical URLs, mirroring how ShellCheck/Clippy point at hosted docs
/// rather than explaining the syntax inline.
const DIRECTIVES_DOCS_URL: &str = "https://github.com/jbearak/raven/blob/main/docs/directives.md";
const DIAGNOSTICS_DOCS_URL: &str = "https://github.com/jbearak/raven/blob/main/docs/diagnostics.md";

/// Build the reframed NSE-discoverability footer for `raven check`'s
/// human-readable `text` output, or `None` when no finding carries an NSE hint.
///
/// Each undefined-variable diagnostic whose flagged identifier sits inside a
/// call argument raven cannot analyze carries a structured `NseHint`
/// (`crate::diagnostic_code`); the hint is NOT in the diagnostic message (see
/// the emitter in `handlers.rs`). This is the *only* place the hint surfaces in
/// output, and it is framed carefully: these findings are *probably real*, so
/// the footer leads with the universal false-positive escape hatches (ignore /
/// expect, as any linter has) and presents the R-specific NSE cause as one
/// possibility — never as raven asserting the call *is* NSE. It still lists the
/// **deduplicated**, copy-pasteable per-function directives (sorted for
/// determinism; one suggestion per function however many findings it caused)
/// and links the docs for the semantics. The machine formats (`json`/`sarif`)
/// and the editor carry neither this footer nor an inline hint (the editor
/// shows just the bare "`x` is not defined"), so this is `text`-only.
fn format_nse_hint_footer(diags: &[(PathBuf, Diagnostic)]) -> Option<String> {
    let mut count = 0usize;
    let mut hints = Vec::new();
    for (_, d) in diags {
        if let Some(hint) = crate::diagnostic_code::undefined_variable_nse_hint(&d.data) {
            count += 1;
            hints.push(hint);
        }
    }
    if count == 0 {
        return None;
    }
    // Aggregate per callee (named-formal directives first, positional pair last,
    // both sorted for determinism). Per-callee aggregation is load-bearing:
    // `# raven: nse` is last-declaration-wins, so one directive per formal would
    // leave only the last in effect — see `nse_footer_directives`.
    let suggestions = crate::diagnostic_code::nse_footer_directives(&hints);
    let has_positional = suggestions.iter().any(|s| s.starts_with("# raven: func"));

    // Number-agnostic lead clause without an awkward "warning(s)". "whose
    // source raven can't see" (not "cannot analyze") makes clear the cause is a
    // missing definition — a package export raven has no body for — not an
    // analysis error on code it does have.
    // Severity-neutral noun ("finding", not "warning"): `raven check` honors
    // `diagnostics.undefinedVariableSeverity`, so these may render as errors,
    // info, or hints above — the footer must not contradict the configured
    // severity.
    let (noun, verb, obj) = if count == 1 {
        (
            "finding",
            "sits",
            "a call to a package function whose source raven can't see",
        )
    } else {
        (
            "findings",
            "sit",
            "calls to package functions whose source raven can't see",
        )
    };
    let mut out = format!(
        "{count} undefined-variable {noun} above {verb} inside {obj}. If one is a \
         false positive, you can suppress it as with any \
         linter (`# raven: ignore`, `# nolint`, or `# raven: expect`). R has one extra cause: a \
         function that captures an argument via non-standard evaluation (NSE) — as \
         `dplyr::filter(df, col > 0)` treats `col` as a column, not a variable — makes a valid \
         name look undefined. Raven already recognizes NSE in many common packages (the \
         tidyverse and more) but not all, so these findings come from functions outside that \
         built-in coverage. If that is the case here, declare the function's NSE contract \
         instead of suppressing:\n"
    );
    for s in &suggestions {
        // A positional suggestion is two directives on separate lines (each
        // `# raven:` directive must own its line); indent every line so the
        // whole pair stays aligned under the footer.
        for line in s.lines() {
            out.push_str("\n  ");
            out.push_str(line);
        }
    }
    if has_positional {
        // Explain why some suggestions carry two directives: a positionally
        // passed argument has no name to key on, so raven needs the function's
        // formal list (`# raven: func`) before it can say which formal is NSE.
        out.push_str(
            "\n\nThe two-line `# raven: func …` / `# raven: nse …` pair is for an argument passed \
             positionally: raven needs the function's parameter list to know which formal the \
             argument is, so fill `<formals>` with the function's signature and `<nse-formals>` \
             with the captured ones. Keep them on separate lines — each `# raven:` directive must \
             be the only one on its line. When the argument is passed by name, naming that formal \
             (`# raven: nse fn(x)`) is enough.",
        );
    }
    out.push_str(&format!(
        "\n\nSee {DIRECTIVES_DOCS_URL} for these directives and {DIAGNOSTICS_DOCS_URL} for \
         handling false positives."
    ));
    Some(out)
}

/// Run the full diagnostic pipeline for one already-opened document. Returns an
/// empty vec when the snapshot can't be built (parse failure or document not
/// open). A malformed file is not an operator error here — its reportable
/// syntax errors are surfaced like any other diagnostic when the tree still
/// builds.
/// The synchronous half of a file's diagnostics: build the snapshot and run the
/// CPU-bound scope-resolution pass. Returns the pre-async findings plus the
/// inputs the async missing-file pass needs (`directive_meta`, severity). Split
/// out so `raven check` can run this — the expensive part — across files in
/// parallel (issue #479 WI3), then do the cheap async filesystem checks
/// afterward. `open_documents` is the worker's coherent one-entry store, while
/// `contextual_providers` adds only provider content that a URI-global graph
/// neighborhood cannot represent (see
/// [`crate::handlers::DiagnosticsSnapshot::build_with_open_documents_and_extra_providers`]).
fn compute_file_diagnostics_sync(
    state: &crate::state::WorldState,
    uri: &Url,
    open_documents: &impl crate::content_provider::OpenDocumentsView,
    contextual_providers: &CliContextualProviderPlan,
) -> Option<(
    Vec<Diagnostic>,
    crate::cross_file::CrossFileMetadata,
    Option<DiagnosticSeverity>,
    crate::cross_file::CaseMismatchSeverity,
)> {
    let snapshot =
        crate::handlers::DiagnosticsSnapshot::build_with_open_documents_and_extra_providers(
            state,
            uri,
            open_documents,
            &contextual_providers.extras,
            contextual_providers.divergence,
        )?;
    let cancel = crate::handlers::DiagCancelToken::never();
    let sync_diags = crate::handlers::diagnostics_from_snapshot(&snapshot, uri, &cancel)?;
    let missing_file_severity = snapshot.cross_file_config.missing_file_severity;
    let case_mismatch_severity = snapshot.cross_file_config.case_mismatch_severity;
    Some((
        sync_diags,
        snapshot.directive_meta,
        missing_file_severity,
        case_mismatch_severity,
    ))
}

/// The async half: replace the snapshot's cache-based missing-file checks with
/// real on-disk existence checks — exactly what the LSP publish path does.
async fn finalize_file_diagnostics(
    state: &crate::state::WorldState,
    uri: &Url,
    sync_diags: Vec<Diagnostic>,
    directive_meta: &crate::cross_file::CrossFileMetadata,
    missing_file_severity: Option<DiagnosticSeverity>,
    case_mismatch_severity: crate::cross_file::CaseMismatchSeverity,
) -> Vec<Diagnostic> {
    if crate::file_type::file_type_from_uri(uri) != crate::file_type::FileType::R {
        return sync_diags;
    }
    crate::handlers::diagnostics_async_standalone(
        uri,
        sync_diags,
        directive_meta,
        state.workspace_folders.first(),
        missing_file_severity,
        case_mismatch_severity,
    )
    .await
}

async fn compute_file_diagnostics(
    state: &mut crate::state::WorldState,
    uri: &Url,
) -> Vec<Diagnostic> {
    let contextual_providers = prepare_cli_contextual_providers(state, uri);
    let Some((sync_diags, directive_meta, missing_file_severity, case_mismatch_severity)) =
        compute_file_diagnostics_sync(state, uri, &state.documents, &contextual_providers)
    else {
        return Vec::new();
    };
    finalize_file_diagnostics(
        state,
        uri,
        sync_diags,
        &directive_meta,
        missing_file_severity,
        case_mismatch_severity,
    )
    .await
}

/// Compute the sorted `(path, Diagnostic)` set for every report target (issue
/// #479 WI3). Returns the diagnostics, the union of attached loaded packages
/// (for the missing-export-metadata warning), and whether any per-target
/// operator error occurred (bad URL / unreadable disk-fallback target).
///
/// The CPU-bound diagnostic pass is parallelized across files: the graph/index
/// caches are `RwLock`/atomic and immutable after the scan, so per-file scope
/// resolution is safe to run concurrently. The one hazard (Codex review) is that
/// open documents outrank index content in the content provider, so sharing
/// `state.documents` across workers would make each worker treat the others'
/// targets as "open" and pull the wrong artifacts. We avoid that entirely:
/// `state.documents` stays empty during the parallel region, and each worker
/// passes a coherent one-entry store holding only its target. Context-sensitive
/// tar providers remain in the single finalized workspace index and are added
/// to snapshot collection without changing graph edges (see
/// `compute_file_diagnostics_sync` /
/// `DiagnosticsSnapshot::build_with_open_documents_and_extra_providers`).
/// This reproduces the
/// sequential "exactly one open target" semantics per task, so output is
/// byte-identical to a sequential run (asserted by
/// `parallel_collection_matches_sequential`). The async on-disk missing-file
/// checks (cheap I/O) and the rare disk-fallback targets (not in the workspace
/// index) are handled afterward on the async runtime.
///
/// Blocking-in-async note: the phase-1 `par_iter().collect()` is a synchronous,
/// CPU-bound rayon join that runs on rayon's own thread pool while blocking the
/// calling tokio worker until it returns. We deliberately do NOT wrap it in
/// `block_in_place`/`spawn_blocking`. `raven check`'s tokio runtime runs exactly
/// one root future (`cli::check::run`, awaited inline from `#[tokio::main]`) with
/// no sibling tasks competing for a worker in this region, so there is nothing
/// for `block_in_place` to migrate — it would be a runtime no-op here while
/// adding a panic-on-`current_thread`-runtime hazard. The process exits right
/// after the report loop. If `raven check` ever gains concurrent tokio tasks
/// around this call, revisit and wrap the rayon phase then.
async fn collect_target_diagnostics(
    state: &mut crate::state::WorldState,
    targets: &[PathBuf],
) -> (
    Vec<(PathBuf, Diagnostic)>,
    std::collections::BTreeSet<String>,
    bool,
) {
    use rayon::prelude::*;

    struct SyncResult {
        path: PathBuf,
        uri: Url,
        sync_diags: Vec<Diagnostic>,
        directive_meta: crate::cross_file::CrossFileMetadata,
        missing_file_severity: Option<DiagnosticSeverity>,
        case_mismatch_severity: crate::cross_file::CaseMismatchSeverity,
        loaded_packages: Vec<String>,
    }

    let mut all_diags: Vec<(PathBuf, Diagnostic)> = Vec::new();
    let mut reported_loaded_packages = std::collections::BTreeSet::new();
    let mut operator_error = false;
    let mut contextual_provider_plans = std::collections::HashMap::new();
    let mut active_conditional_packages = std::collections::HashMap::new();
    for path in targets {
        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };
        if state.workspace_index.contains(&uri) {
            let plan = prepare_cli_contextual_providers(state, &uri);
            contextual_provider_plans.insert(uri.clone(), plan);
            active_conditional_packages.insert(
                uri.clone(),
                active_conditional_packages_for_uri(state, &uri),
            );
        }
    }

    // Phase 1 (parallel, CPU-bound): indexed targets only. A target that is not
    // in the workspace index (disk-fallback) or whose path can't be a URL is
    // skipped here and handled sequentially below.
    let sync_results: Vec<SyncResult> = targets
        .par_iter()
        .filter_map(|path| {
            let uri = Url::from_file_path(path).ok()?;
            // Reuse the already-parsed `Document` from the scan (tree included),
            // exactly as the sequential path did — no disk re-read / re-parse.
            let entry = state.workspace_index.get(&uri)?;
            let doc = crate::state::document_from_workspace_entry(
                &uri,
                &entry,
                crate::chunks::classify_chunk_document(uri.path()),
            );
            let mut loaded_packages: Vec<String> = doc.loaded_packages.to_vec();
            if let Some(active) = active_conditional_packages.get(&uri) {
                loaded_packages.extend(active.iter().cloned());
            }
            let mut open_documents = crate::open_document_store::OpenDocumentStore::new();
            open_documents.open_prepared(
                uri.clone(),
                doc,
                entry.metadata.clone(),
                entry.artifacts.clone(),
                None,
            );
            let empty_plan = CliContextualProviderPlan::default();
            let contextual_providers = contextual_provider_plans.get(&uri).unwrap_or(&empty_plan);
            let (sync_diags, directive_meta, missing_file_severity, case_mismatch_severity) =
                compute_file_diagnostics_sync(state, &uri, &open_documents, contextual_providers)?;
            Some(SyncResult {
                path: path.clone(),
                uri,
                sync_diags,
                directive_meta,
                missing_file_severity,
                case_mismatch_severity,
                loaded_packages,
            })
        })
        .collect();

    // Phase 2 (async): finalize the parallel results with on-disk missing-file
    // checks and collect their attached packages. Order is irrelevant — the
    // whole `all_diags` set is sorted below.
    for r in sync_results {
        reported_loaded_packages.extend(r.loaded_packages);
        let diags = finalize_file_diagnostics(
            state,
            &r.uri,
            r.sync_diags,
            &r.directive_meta,
            r.missing_file_severity,
            r.case_mismatch_severity,
        )
        .await;
        for d in diags {
            all_diags.push((r.path.clone(), d));
        }
    }

    // Phase 3 (sequential): targets not handled in phase 1 — a bad URL, or a
    // disk-fallback target the scan didn't index (e.g. a symlink-alias path, or
    // an `.Rmd`/`.Rmarkdown`/`.qmd` chunk file outside the R-only scan). Rare;
    // the existing open → compute → close path (which uses `state.documents`)
    // is preserved verbatim, so behavior for these is unchanged.
    for path in targets {
        let Ok(uri) = Url::from_file_path(path) else {
            eprintln!(
                "raven check: cannot convert path to URL: {}",
                path.display()
            );
            operator_error = true;
            continue;
        };
        if state.workspace_index.contains(&uri) {
            continue; // already handled in the parallel phase
        }
        let text = match crate::state::read_source(path) {
            Ok(t) => t,
            Err(crate::state::SourceReadError::Io(e)) => {
                eprintln!("raven check: cannot read {}: {e}", path.display());
                operator_error = true;
                continue;
            }
            // A mis-encoded file is a property of the code, like a syntax
            // error — not an operator error. Report it as a finding (see
            // `encoding_diagnostic`) and keep going, so the exit code
            // reflects findings rather than a half-read abort.
            Err(crate::state::SourceReadError::InvalidEncoding { offset, byte }) => {
                all_diags.push((path.clone(), encoding_diagnostic(offset, byte)));
                continue;
            }
        };
        open_disk_fallback_target(state, &uri, path, &text);
        let mut loaded_packages = state
            .documents
            .get(&uri)
            .map(|doc| doc.loaded_packages.clone())
            .unwrap_or_default();
        loaded_packages.extend(active_conditional_packages_for_uri(state, &uri));
        reported_loaded_packages.extend(loaded_packages);
        let diags = compute_file_diagnostics(state, &uri).await;
        state.close_document(&uri);
        for d in diags {
            all_diags.push((path.clone(), d));
        }
    }

    // Deterministic output regardless of the scan's parallel HashMap order.
    all_diags.sort_by(|(pa, da), (pb, db)| {
        pa.cmp(pb)
            .then(da.range.start.line.cmp(&db.range.start.line))
            .then(da.range.start.character.cmp(&db.range.start.character))
    });

    (all_diags, reported_loaded_packages, operator_error)
}

/// Resolve which files to report diagnostics for. Empty `paths` means every R
/// source, chunk-bearing document, and supported standalone JAGS or Stan model
/// under the workspace root. Explicit paths are taken as-is (files) or walked
/// (directories). The result is sorted and de-duplicated for stable output.
/// Chunk files are collected both as explicit args and while walking a
/// directory, so `raven check` diagnoses the R chunks inside them (issue #343).
///
/// Every resolved path is canonicalized so its file URI matches the canonical
/// `root` used to build the dependency graph. Without this, on platforms where
/// the workspace root resolves through a symlink (e.g. macOS `/var` →
/// `/private/var`), an explicitly-passed `/var/...` path would neither match
/// the graph's `/private/var/...` keys (silently breaking cross-file
/// resolution) nor pass the workspace-boundary check.
///
/// A named path that does not exist sets `*operator_error`, so the caller can
/// return exit code 2 — matching `raven lint`.
#[cfg(test)]
fn collect_report_targets(
    paths: &[PathBuf],
    root: &Path,
    operator_error: &mut bool,
) -> Vec<PathBuf> {
    let exclusions = crate::config_file::CompiledWorkspaceExclusions::default();
    collect_report_targets_with_exclusions(paths, root, &exclusions, operator_error)
}

fn collect_report_targets_with_exclusions(
    paths: &[PathBuf],
    root: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
    operator_error: &mut bool,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if paths.is_empty() {
        collect_check_target_paths_with_exclusions(root, &mut out, exclusions);
    } else {
        for p in paths {
            let abs = absolute_path(root, p);
            let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
            if abs.is_dir() {
                collect_check_target_paths_with_exclusions(&abs, &mut out, exclusions);
            } else if abs.is_file() {
                if is_r_file(&abs)
                    || is_chunk_file(&abs)
                    || is_jags_file(&abs)
                    || is_stan_file(&abs)
                {
                    out.push(abs);
                }
                // Other file types: silently ignored, matching lint's walk.
            } else {
                eprintln!("raven check: path does not exist: {}", p.display());
                *operator_error = true;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn run_blocking(args: CheckArgs) -> i32 {
        tokio::runtime::Runtime::new().unwrap().block_on(run(args))
    }

    // ---- degraded_env_note (startup-note decision) ------------------------
    //
    // End-to-end coverage is impossible on a machine with R installed:
    // `get_fallback_lib_paths()` finds the framework library, so the build
    // classifies `Ready` and `RNotFound`/`NoLibraryPaths` are never reached.
    // These pure-function tests pin the decision deterministically instead.
    use crate::package_library::PackageLibraryStatus as S;

    #[test]
    fn degraded_env_note_silent_when_ready_or_disabled() {
        // Live R / package awareness off: never degraded, regardless of providers.
        assert!(degraded_env_note(&S::Ready, false).is_none());
        assert!(degraded_env_note(&S::Ready, true).is_none());
        assert!(degraded_env_note(&S::Disabled, false).is_none());
    }

    #[test]
    fn degraded_env_note_suppressed_when_offline_packages_present() {
        // The user's CI report: R absent but `raven packages update`/`freeze`
        // already loaded offline coverage — the warning would be a false alarm,
        // and would tell CI to re-run the command it already ran.
        for status in [
            S::RNotFound,
            S::InitFailed("boom".into()),
            S::NoLibraryPaths,
        ] {
            assert!(
                degraded_env_note(&status, true).is_none(),
                "note must be suppressed for {status:?} when offline packages loaded"
            );
        }
    }

    #[test]
    fn degraded_env_note_fires_with_remedy_when_coverage_is_base_only() {
        // No offline data: coverage really is base-R-only, so each cause names
        // itself, the consequence (limited to base R), and the remedy.
        let r_not_found = degraded_env_note(&S::RNotFound, false).expect("note");
        assert!(r_not_found.starts_with("R not found on PATH;"));
        assert!(r_not_found.contains("limited to base R"));
        assert!(r_not_found.contains("raven packages update"));
        // No `raven check:` prefix — the caller adds it (footer notes don't).
        assert!(!r_not_found.starts_with("raven check:"));

        let init_failed = degraded_env_note(&S::InitFailed("xyz".into()), false).expect("note");
        assert!(
            init_failed.starts_with("R found but its package library failed to initialize (xyz);")
        );
        assert!(init_failed.contains("raven packages update"));

        let no_lib = degraded_env_note(&S::NoLibraryPaths, false).expect("note");
        assert!(no_lib.starts_with("R found but no library paths were discovered;"));
        assert!(no_lib.contains("raven packages update"));
    }

    fn run_with_cwd_blocking(args: CheckArgs, cwd: &Path) -> i32 {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_with_cwd(args, cwd))
    }

    fn base_args(workspace: &Path) -> CheckArgs {
        CheckArgs {
            paths: Vec::new(),
            workspace: Some(workspace.to_path_buf()),
            config_path: None,
            no_config: true,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Info,
            quiet: true,
            color: ColorChoice::Never,
            report_uninstalled: false,
        }
    }

    fn model_diagnostic_args(workspace: &Path, settings: &str) -> CheckArgs {
        fs::write(
            workspace.join("raven.toml"),
            format!("[diagnostics]\n{settings}"),
        )
        .unwrap();
        let mut args = base_args(workspace);
        args.no_config = false;
        args
    }

    #[test]
    fn parse_report_uninstalled_flag() {
        let args = parse_args(["--report-uninstalled".to_string()].into_iter()).unwrap();
        assert!(args.report_uninstalled);

        let default = parse_args(std::iter::empty()).unwrap();
        assert!(!default.report_uninstalled);
    }

    fn nse_diag(callee: &str, formal: Option<&str>) -> (PathBuf, Diagnostic) {
        // The hint rides `data` only — the message is clean (production no longer
        // appends an inline suffix), exactly as the footer builder reads it.
        let hint = crate::diagnostic_code::NseHint {
            callee: callee.to_string(),
            dir: callee.to_string(),
            formal: formal.map(str::to_string),
        };
        (
            PathBuf::from("main.R"),
            Diagnostic {
                message: format!("{callee}_arg is not defined"),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                data: crate::diagnostic_code::undefined_variable_data(false, Some(&hint)),
                ..Default::default()
            },
        )
    }

    fn plain_undefined_diag() -> (PathBuf, Diagnostic) {
        (
            PathBuf::from("main.R"),
            Diagnostic {
                message: "z is not defined".into(),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                ..Default::default()
            },
        )
    }

    #[test]
    fn nse_footer_is_none_without_any_hint() {
        assert!(super::format_nse_hint_footer(&[]).is_none());
        assert!(super::format_nse_hint_footer(&[plain_undefined_diag()]).is_none());
    }

    #[test]
    fn nse_footer_dedups_suggestions_but_counts_every_finding() {
        // Three findings: two on `aes(x = ...)` (one suggestion) and one
        // positional `facet_wrap(...)`. The count reflects all three findings;
        // the suggestions deduplicate to two, sorted deterministically.
        let diags = vec![
            nse_diag("aes", Some("x")),
            nse_diag("aes", Some("x")),
            nse_diag("facet_wrap", None),
            plain_undefined_diag(),
        ];
        let footer = super::format_nse_hint_footer(&diags).expect("footer present");

        assert!(
            footer.contains(
                "3 undefined-variable findings above sit inside calls to package functions whose source raven can't see"
            ),
            "count + plural lead clause: {footer}"
        );
        // The coverage-gap note explains why only some package calls are flagged.
        assert!(
            footer.contains("recognizes NSE in many common packages")
                && footer.contains("but not all"),
            "footer acknowledges built-in NSE coverage and its gaps: {footer}"
        );
        // Reframed: universal escape hatches first, NSE as one possibility.
        assert!(
            footer.contains("# raven: ignore")
                && footer.contains("# nolint")
                && footer.contains("# raven: expect"),
            "footer offers the universal suppression directives: {footer}"
        );
        assert!(
            footer.contains("non-standard evaluation (NSE)"),
            "footer names the R-specific NSE cause: {footer}"
        );
        // The deduplicated, copy-pasteable directives — exactly one `aes` line.
        assert_eq!(
            footer.matches("# raven: nse aes(x)").count(),
            1,
            "aes suggestion appears once despite two findings: {footer}"
        );
        assert!(
            footer.contains("# raven: func facet_wrap(<formals>)")
                && footer.contains("# raven: nse facet_wrap(<nse-formals>)"),
            "positional suggestion present (two directives, separate lines): {footer}"
        );
        // Named form first, positional (two-directive) form last.
        assert!(
            footer.find("# raven: nse aes(x)").unwrap()
                < footer.find("# raven: func facet_wrap").unwrap(),
            "named suggestion sorts before positional: {footer}"
        );
        // The positional case is explained (why it needs two directives).
        assert!(
            footer.contains("needs the function's parameter list"),
            "footer explains the positional two-directive form: {footer}"
        );
        // Both docs URLs are present (directives + handling false positives).
        assert!(
            footer.contains("https://github.com/jbearak/raven/blob/main/docs/directives.md")
                && footer
                    .contains("https://github.com/jbearak/raven/blob/main/docs/diagnostics.md"),
            "docs URLs present: {footer}"
        );
    }

    #[test]
    fn nse_footer_uses_singular_lead_clause_for_one_finding() {
        let footer =
            super::format_nse_hint_footer(&[nse_diag("aes", Some("x"))]).expect("footer present");
        assert!(
            footer.contains(
                "1 undefined-variable finding above sits inside a call to a package function whose source raven can't see"
            ),
            "singular lead clause: {footer}"
        );
        // No positional suggestion here, so the two-directive explanation is omitted.
        assert!(
            !footer.contains("needs the function's parameter list"),
            "named-only footer omits the positional explanation: {footer}"
        );
    }

    #[test]
    fn footer_goes_to_stdout_for_text_format() {
        // Text output is human-readable and shares the terminal/CI stream with
        // the diagnostics, so the context-note footer must ride the SAME stream
        // (stdout) — otherwise a merged consumer (GitHub Actions, `2>&1`)
        // interleaves it with the findings it annotates.
        assert_eq!(
            super::footer_stream(OutputFormat::Text),
            super::FooterStream::Stdout
        );
    }

    #[test]
    fn footer_goes_to_stderr_for_machine_formats() {
        // json/sarif reserve stdout for the machine document, so the footer
        // stays on stderr where it cannot corrupt the parsed output.
        for fmt in [OutputFormat::Json, OutputFormat::Sarif] {
            assert_eq!(super::footer_stream(fmt), super::FooterStream::Stderr);
        }
    }

    #[test]
    fn traversal_budget_note_none_when_no_truncation() {
        let state = crate::state::WorldState::new();
        assert!(super::format_traversal_budget_note(&state).is_none());
    }

    #[test]
    fn traversal_budget_note_fires_on_visited_truncation() {
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        use url::Url;

        let mut state = crate::state::WorldState::new();
        state.cross_file_config.max_transitive_dependents_visited = 1;
        let root = Url::parse("file:///p").unwrap();
        let a = Url::parse("file:///p/a.R").unwrap();
        let meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "b.R".to_string(),
                line: 1,
                column: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        state
            .cross_file_graph
            .update_file(&a, &meta, Some(&root), |_| None);
        // A budget of 1 with an a -> b edge truncates the neighborhood walk.
        let _ = state.cross_file_graph.collect_neighborhood(&a, 64, 1);

        let note = super::format_traversal_budget_note(&state)
            .expect("a truncated traversal must produce a note");
        assert!(note.contains("maxTransitiveDependentsVisited = 1"));
        assert!(note.contains("may be false"));
    }

    #[test]
    fn formats_missing_metadata_warning_for_absent_tier3() {
        use crate::package_library::ShippedDbLoad;
        let msg = super::format_missing_export_metadata_warning(
            &["foo".into(), "bar".into()],
            ShippedDbLoad::Absent,
        );
        // Names are sorted, so order is deterministic.
        assert!(msg.contains("couldn't load exported symbols for bar, foo"));
        assert!(msg.contains("install these packages in your R library"));
        // Absent steers to `update` (then `freeze`) as the CI fallback.
        assert!(msg.contains("run `raven packages update` before `raven check`"));
        assert!(msg.contains("raven packages freeze"));
        assert!(!msg.contains("Tier"));
    }

    #[test]
    fn formats_missing_metadata_warning_absent_tier3_singular() {
        use crate::package_library::ShippedDbLoad;
        let msg =
            super::format_missing_export_metadata_warning(&["foo".into()], ShippedDbLoad::Absent);
        assert!(msg.contains("couldn't load exported symbols for foo"));
        assert!(msg.contains("install this package in your R library"));
        assert!(msg.contains("where it's installed"));
        assert!(!msg.contains("Tier"));
    }

    #[test]
    fn formats_missing_metadata_warning_for_present_tier3_miss() {
        use crate::package_library::ShippedDbLoad;
        let msg =
            super::format_missing_export_metadata_warning(&["foo".into()], ShippedDbLoad::Loaded);
        assert!(msg.contains("couldn't load exported symbols for foo"));
        // Singular wording for a single package.
        assert!(msg.contains("install this package in your R library"));
        assert!(msg.contains("Raven's package symbol database doesn't provide it"));
        assert!(msg.contains("raven packages freeze"));
        assert!(!msg.contains("Tier"));
    }

    #[test]
    fn formats_missing_metadata_warning_for_failed_tier3() {
        use crate::package_library::ShippedDbLoad;
        let msg =
            super::format_missing_export_metadata_warning(&["foo".into()], ShippedDbLoad::Failed);
        assert!(msg.contains("couldn't load exported symbols for foo"));
        // A present-but-unusable DB: explain that, and steer toward re-downloading
        // a good copy rather than freezing project metadata.
        assert!(msg.contains("present but failed to load"));
        assert!(msg.contains("Run `raven packages update` to re-download"));
        assert!(
            !msg.contains("raven packages freeze"),
            "freeze is wrong advice when the DB merely failed to load: {msg}"
        );
        // Self-contained: must not cross-reference the separately-emitted load
        // note by position. The load note and this footer can land on different
        // streams a CI consumer reorders, so "see the load note above" can't be
        // relied on; the load-note detail rides the same footer instead.
        // (The shared head's "Undefined variable warnings above" is fine — those
        // diagnostics share this footer's stream for `text`.)
        assert!(
            !msg.contains("load note above") && !msg.contains("see the load note"),
            "warning must not reference the load note by position: {msg}"
        );
    }

    #[test]
    fn missing_metadata_gate_respects_packages_disabled() {
        let mut state = crate::state::WorldState::new();
        state.cross_file_config.packages_enabled = false;
        let diags = vec![(
            PathBuf::from("main.R"),
            Diagnostic {
                message: "missing_fun is not defined".into(),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                ..Default::default()
            },
        )];

        assert!(!super::should_check_missing_export_metadata(&state, &diags));
    }

    #[test]
    fn missing_metadata_gate_ignores_defined_later_diagnostics() {
        let mut state = crate::state::WorldState::new();
        state.cross_file_config.packages_enabled = true;
        let defined_later = vec![(
            PathBuf::from("main.R"),
            Diagnostic {
                message: "x is used before it is defined (defined on line 3)".into(),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                // Emitters tag the position variants here; the gate keys on it.
                data: crate::diagnostic_code::undefined_variable_data(true, None),
                ..Default::default()
            },
        )];
        assert!(!super::should_check_missing_export_metadata(
            &state,
            &defined_later
        ));

        // The other position/ordering variant: a symbol defined in a sourced
        // file but used before the source() call. It carries the same
        // undefined-variable code and position-variant tag, and is not
        // package-metadata-sensitive either.
        let sourced_later = vec![(
            PathBuf::from("main.R"),
            Diagnostic {
                message: "'j' is used before it's available (sourced on line 5)".into(),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                data: crate::diagnostic_code::undefined_variable_data(true, None),
                ..Default::default()
            },
        )];
        assert!(!super::should_check_missing_export_metadata(
            &state,
            &sourced_later
        ));

        let package_sensitive = vec![(
            PathBuf::from("main.R"),
            Diagnostic {
                message: "mutate is not defined".into(),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                ..Default::default()
            },
        )];
        assert!(super::should_check_missing_export_metadata(
            &state,
            &package_sensitive
        ));
    }

    #[test]
    fn missing_metadata_gate_handles_backtick_name_containing_position_variant_text() {
        // A pathological but valid R non-syntactic name whose text embeds the
        // position-variant prose AND its parentheticals, e.g.
        // `is used before it is defined (defined on line 3)`. Its PLAIN
        // (genuinely-undefined) message would then read
        // "`is used before it is defined (defined on line 3)` is not defined".
        // A message-substring gate would misclassify it as a position variant
        // and skip the package-metadata check. Because the gate now keys on the
        // structured `data` tag (absent here — this is a genuine miss), the
        // symbol name cannot spoof it: this is correctly package-sensitive.
        let mut state = crate::state::WorldState::new();
        state.cross_file_config.packages_enabled = true;
        let plain_backtick = vec![(
            PathBuf::from("main.R"),
            Diagnostic {
                message: "`is used before it is defined (defined on line 3)` is not defined".into(),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    crate::diagnostic_code::UNDEFINED_VARIABLE.to_string(),
                )),
                // No position-variant tag → a genuine miss, package-sensitive.
                data: None,
                ..Default::default()
            },
        )];
        assert!(super::should_check_missing_export_metadata(
            &state,
            &plain_backtick
        ));
    }

    #[test]
    fn parse_defaults() {
        let args = parse_args(Vec::<String>::new().into_iter()).unwrap();
        assert!(args.paths.is_empty());
        assert_eq!(args.workspace, None);
        assert_eq!(args.format, OutputFormat::Text);
        assert_eq!(args.max_severity, SeverityLevel::Info);
        assert!(!args.no_config);
        assert_eq!(args.color, ColorChoice::Auto);
    }

    #[test]
    fn parse_color_and_no_color_alias() {
        let always = parse_args(["--color", "always"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(always.color, ColorChoice::Always);
        let never = parse_args(["--no-color"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(never.color, ColorChoice::Never);
        // Last-one-wins on conflict: `--no-color --color always` ⇒ always.
        let conflict = parse_args(
            ["--no-color", "--color", "always"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();
        assert_eq!(conflict.color, ColorChoice::Always);
        // A bad --color value is a usage error.
        assert!(parse_args(["--color", "sometimes"].iter().map(|s| s.to_string())).is_err());
    }

    #[test]
    fn parse_workspace_and_paths() {
        let args = parse_args(
            ["--workspace", "/tmp/ws", "R/foo.R", "R/bar.R"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();
        assert_eq!(args.workspace, Some(PathBuf::from("/tmp/ws")));
        assert_eq!(
            args.paths,
            vec![PathBuf::from("R/foo.R"), PathBuf::from("R/bar.R")]
        );
    }

    #[test]
    fn parse_format_and_severity() {
        let args = parse_args(
            ["--format", "sarif", "--max-severity", "error"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();
        assert_eq!(args.format, OutputFormat::Sarif);
        assert_eq!(args.max_severity, SeverityLevel::Error);
    }

    #[test]
    fn parse_help_and_unknown_flag() {
        assert_eq!(
            parse_args(["--help"].iter().map(|s| s.to_string())).unwrap_err(),
            "HELP"
        );
        assert!(parse_args(["--bogus"].iter().map(|s| s.to_string())).is_err());
    }

    #[test]
    fn resolve_project_config_honors_explicit_lintr() {
        let tmp = TempDir::new().unwrap();
        let lintr = tmp.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (settings, path) =
            resolve_project_config(false, Some(&lintr), tmp.path(), tmp.path()).unwrap();

        assert_eq!(path.as_deref(), Some(lintr.as_path()));
        assert_eq!(
            settings.unwrap()["linting"]["lineLength"],
            serde_json::json!(120)
        );
    }

    #[test]
    fn resolve_project_config_relative_explicit_lintr_loads_from_workspace() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (settings, path) = resolve_project_config(
            false,
            Some(Path::new("config/.lintr")),
            tmp.path(),
            tmp.path(),
        )
        .unwrap();

        assert_eq!(path.as_deref(), Some(config_dir.join(".lintr").as_path()));
        assert_eq!(
            settings.unwrap()["linting"]["lineLength"],
            serde_json::json!(120)
        );
    }

    #[test]
    fn resolve_project_config_relative_explicit_lintr_loads_from_invocation_cwd() {
        let invocation = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config_dir = invocation.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();

        let (settings, path) = resolve_project_config_with_options(
            false,
            Some(Path::new("config/.lintr")),
            workspace.path(),
            invocation.path(),
            &crate::config_file::DiscoveryOptions::default(),
        )
        .unwrap();

        assert_eq!(path.as_deref(), Some(config_dir.join(".lintr").as_path()));
        assert_eq!(
            settings.unwrap()["linting"]["lineLength"],
            serde_json::json!(120)
        );
    }

    #[test]
    fn resolve_project_config_ignores_literal_home_lintr_by_default() {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            home.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let discovery_options =
            crate::config_file::DiscoveryOptions::default().with_home_dir(home.path());

        let (settings, path) = resolve_project_config_with_options(
            false,
            None,
            &workspace,
            &workspace,
            &discovery_options,
        )
        .unwrap();

        assert!(
            settings.is_none(),
            "CLI auto-discovery should ignore literal home .lintr by default"
        );
        assert!(path.is_none());
    }

    #[test]
    fn end_to_end_explicit_lintr_finds_line_length_violation() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(20))\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("over.R"),
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        let mut args = base_args(tmp.path());
        args.no_config = false;
        args.config_path = Some(tmp.path().join(".lintr"));
        args.paths = vec![tmp.path().join("over.R")];
        args.max_severity = SeverityLevel::Off;

        assert_eq!(run_blocking(args), EXIT_LINT_FAILED);
    }

    #[test]
    fn end_to_end_relative_explicit_lintr_uses_invocation_cwd_not_workspace() {
        let invocation = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config_dir = invocation.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(20))\n",
        )
        .unwrap();
        let target = workspace.path().join("over.R");
        fs::write(
            &target,
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        let mut args = base_args(workspace.path());
        args.no_config = false;
        args.config_path = Some(PathBuf::from("config/.lintr"));
        args.paths = vec![target];
        args.max_severity = SeverityLevel::Off;

        assert_eq!(
            run_with_cwd_blocking(args, invocation.path()),
            EXIT_LINT_FAILED
        );
    }

    #[test]
    fn collect_targets_empty_walks_workspace() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.R"), "x <- 1\n").unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(tmp.path().join("R/b.r"), "y <- 2\n").unwrap();
        fs::write(tmp.path().join("notes.Rmd"), "prose\n").unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".git/c.R"), "z <- 3\n").unwrap();

        let mut operator_error = false;
        let targets = collect_report_targets(&[], tmp.path(), &mut operator_error);
        // Two .R/.r files + the .Rmd (its R chunks are diagnosed, #343); .git
        // skipped.
        assert_eq!(targets.len(), 3, "got {targets:?}");
        assert!(targets.iter().all(|p| is_r_file(p) || is_chunk_file(p)));
        assert!(targets.iter().any(|p| is_chunk_file(p)));
        assert!(!operator_error);
    }

    #[test]
    fn collect_targets_empty_skips_project_excluded_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("included.R"), "x <- 1\n").unwrap();
        fs::create_dir(tmp.path().join("generated")).unwrap();
        fs::write(tmp.path().join("generated/ignored.R"), "y <- 1\n").unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &serde_json::json!({ "workspace": { "exclude": ["generated/**"] } }),
            vec![tmp.path().to_path_buf()],
        );

        let mut operator_error = false;
        let targets = collect_report_targets_with_exclusions(
            &[],
            tmp.path(),
            &exclusions,
            &mut operator_error,
        );

        assert_eq!(targets.len(), 1, "got {targets:?}");
        assert!(targets[0].ends_with("included.R"), "got {targets:?}");
        assert!(!operator_error);
    }

    #[test]
    fn package_data_excluded_r_script_does_not_seed_dataset_scope() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(
            root.join("raven.toml"),
            "[workspace]\nexclude = [\"data/**\"]\n[packages]\nenabled = false\n",
        )
        .unwrap();
        fs::create_dir(root.join("R")).unwrap();
        fs::create_dir(root.join("data")).unwrap();
        fs::write(
            root.join("data").join("excluded.R"),
            "excluded_dataset <- 1\n",
        )
        .unwrap();
        let target = root.join("R").join("use.R");
        fs::write(&target, "f <- function() excluded_dataset\n").unwrap();

        let mut args = base_args(root);
        args.no_config = false;
        args.paths = vec![target];
        let diags = collect_diagnostics_blocking(&args);

        assert!(
            has_undefined(&diags, "excluded_dataset"),
            "excluded data/*.R symbol must remain undefined; got {diags:?}"
        );
    }

    #[test]
    fn package_data_raw_excluded_script_does_not_seed_sysdata_scope() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(
            root.join("raven.toml"),
            "[workspace]\nexclude = [\"data-raw/**\"]\n[packages]\nenabled = false\n",
        )
        .unwrap();
        fs::create_dir(root.join("R")).unwrap();
        fs::create_dir(root.join("data-raw")).unwrap();
        fs::write(
            root.join("data-raw").join("generate.R"),
            "usethis::use_data(excluded_sysdata, internal = TRUE)\n",
        )
        .unwrap();
        let target = root.join("R").join("use.R");
        fs::write(&target, "f <- function() excluded_sysdata\n").unwrap();

        let mut args = base_args(root);
        args.no_config = false;
        args.paths = vec![target];
        let diags = collect_diagnostics_blocking(&args);

        assert!(
            has_undefined(&diags, "excluded_sysdata"),
            "excluded data-raw/*.R sysdata symbol must remain undefined; got {diags:?}"
        );
    }

    #[test]
    fn walk_includes_rmd_rmarkdown_and_qmd() {
        // The empty-PATHS workspace walk collects chunk-bearing documents
        // alongside R sources, including mixed-case extensions
        // (is_chunk_file matches `.rmd`/`.rmarkdown`/`.qmd`
        // case-insensitively).
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.R"), "x <- 1\n").unwrap();
        fs::write(tmp.path().join("report.Rmd"), "prose\n").unwrap();
        fs::write(tmp.path().join("notebook.Rmarkdown"), "prose\n").unwrap();
        fs::write(tmp.path().join(".Rmarkdown"), "prose\n").unwrap();
        fs::write(tmp.path().join("paper.qmd"), "prose\n").unwrap();
        fs::write(tmp.path().join("UPPER.RMD"), "prose\n").unwrap();
        fs::write(tmp.path().join("UPPER.RMARKDOWN"), "prose\n").unwrap();
        fs::write(tmp.path().join("UPPER.QMD"), "prose\n").unwrap();

        let mut operator_error = false;
        let targets = collect_report_targets(&[], tmp.path(), &mut operator_error);
        assert_eq!(targets.len(), 8, "got {targets:?}");
        let chunk_count = targets.iter().filter(|p| is_chunk_file(p)).count();
        assert_eq!(
            chunk_count, 7,
            "all seven chunk files collected; got {targets:?}"
        );
        assert!(!operator_error);
    }

    #[test]
    fn directory_walk_rmarkdown_chunk_syntax_error_exits_failed() {
        let tmp = TempDir::new().unwrap();
        let rmarkdown = tmp.path().join("report.Rmarkdown");
        fs::write(&rmarkdown, "# Title\n```{r}\nf <- function( {\n```\n").unwrap();

        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().to_path_buf()];
        assert_eq!(run_blocking(args.clone()), EXIT_LINT_FAILED);

        let diags = collect_diagnostics_blocking(&args);
        let canonical_rmarkdown = std::fs::canonicalize(&rmarkdown).unwrap();
        assert!(
            diags.iter().any(|(path, d)| path == &canonical_rmarkdown
                && d.range.start.line == 2
                && d.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)),
            "directory walk must analyze .Rmarkdown chunks at document coordinates; got {diags:?}"
        );
    }

    #[test]
    fn collect_targets_explicit_chunk_file_included() {
        let tmp = TempDir::new().unwrap();
        let rmd = tmp.path().join("report.Rmd");
        fs::write(&rmd, "prose\n").unwrap();
        let mut operator_error = false;
        let targets =
            collect_report_targets(std::slice::from_ref(&rmd), tmp.path(), &mut operator_error);
        // Targets are canonicalized so they match the canonical workspace root.
        assert_eq!(targets, vec![std::fs::canonicalize(&rmd).unwrap()]);
        assert!(!operator_error);
    }

    #[test]
    fn explicit_file_target_bypasses_project_exclusion() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[workspace]\nexclude = [\"generated/**\"]\n",
        )
        .unwrap();
        fs::create_dir(tmp.path().join("generated")).unwrap();
        let target = tmp.path().join("generated/explicit.R");
        fs::write(&target, "definitely_missing_symbol_559\n").unwrap();
        let canonical_target = std::fs::canonicalize(&target).unwrap();

        let mut args = base_args(tmp.path());
        args.no_config = false;
        args.paths = vec![target.clone()];

        let diags = collect_diagnostics_blocking(&args);
        assert!(
            diags.iter().any(|(path, diag)| path == &canonical_target
                && diag
                    .message
                    .contains("definitely_missing_symbol_559 is not defined")),
            "explicit excluded file must still be diagnosed; got {diags:?}"
        );
    }

    #[test]
    fn indentation_advice_requires_cli_producer_config() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("main.R");
        fs::write(&target, "result <- foo() +\n          bar()\n").unwrap();

        let linting = r#"
[linting]
enabled = true
indentationUnit = 2
indentationSeverity = "information"
infixContinuationStyle = "indented"
"#;
        fs::write(
            tmp.path().join("raven.toml"),
            format!(
                "{linting}\n[indentation]\nenabled = true\ninfixContinuationStyle = \"aligned\"\n"
            ),
        )
        .unwrap();

        let mut args = base_args(tmp.path());
        args.no_config = false;
        args.paths = vec![target];
        let with_policy = collect_diagnostics_blocking(&args);
        let with_policy_message = with_policy
            .iter()
            .find(|(_, diagnostic)| {
                diagnostic.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        crate::linting::rule_ids::INDENTATION.to_string(),
                    ))
            })
            .map(|(_, diagnostic)| diagnostic.message.as_str())
            .expect("configured indentation violation must be reported");
        assert!(
            with_policy_message.contains(
                "Column 10 matches `raven.indentation.infixContinuationStyle = \"aligned\"`"
            ),
            "explicit CLI producer policy must add advice: {with_policy_message}"
        );

        fs::write(tmp.path().join("raven.toml"), linting).unwrap();
        let without_policy = collect_diagnostics_blocking(&args);
        let without_policy_message = without_policy
            .iter()
            .find(|(_, diagnostic)| {
                diagnostic.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        crate::linting::rule_ids::INDENTATION.to_string(),
                    ))
            })
            .map(|(_, diagnostic)| diagnostic.message.as_str())
            .expect("configured indentation violation must still be reported");
        assert_eq!(
            without_policy_message,
            "Indentation should be 2 spaces, not 10."
        );
    }

    #[test]
    fn collect_targets_nonexistent_flags_operator_error() {
        let tmp = TempDir::new().unwrap();
        let mut operator_error = false;
        let targets = collect_report_targets(
            &[PathBuf::from("no_such_file.R")],
            tmp.path(),
            &mut operator_error,
        );
        assert!(targets.is_empty());
        assert!(operator_error);
    }

    #[test]
    fn nonexistent_path_exits_operator_error() {
        let tmp = TempDir::new().unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().join("missing.R")];
        assert_eq!(run_blocking(args), EXIT_OPERATOR_ERROR);
    }

    #[test]
    fn explicit_file_resolves_cross_file_scope() {
        // Regression: an explicitly-passed file must canonicalize so its URI
        // matches the canonical dependency-graph keys. Otherwise `add_one`
        // (defined in a sourced sibling) would be flagged undefined and the
        // sourced path would read as "outside workspace".
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(
            tmp.path().join("R/helpers.R"),
            "add_one <- function(x) x + 1\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("R/main.R"),
            "source(\"helpers.R\")\nresult <- add_one(41)\n",
        )
        .unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().join("R/main.R")];
        // A clean cross-file reference must not trip any diagnostic.
        assert_eq!(run_blocking(args), EXIT_OK);
    }
    #[test]
    fn namespace_only_package_root_does_not_get_package_internal_scope() {
        // Single-signal `Auto` activation (commit ecb8c19a; r-package-mode
        // architecture spec §6.1) requires a workspace-root DESCRIPTION with a
        // non-empty `Package:` field — R's own definition of a package.
        // NAMESPACE / R/ presence is NOT an activation heuristic (a NAMESPACE
        // without `Package:` is meaningless to R itself, and broadening
        // activation biases toward wrongly suppressing real diagnostics in
        // non-packages). So a NAMESPACE-only root runs in SCRIPT mode: R/*.R
        // files do not share a package-internal scope, and a sibling helper
        // referenced without an explicit source()/import reads as undefined —
        // exactly the Phase 5 behavior §6.1/§11.1 prescribes.
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(tmp.path().join("NAMESPACE"), "export(helper_fn)\n").unwrap();
        fs::write(
            tmp.path().join("R/helper.R"),
            "helper_fn <- function(x) x + 1\n",
        )
        .unwrap();
        fs::write(tmp.path().join("R/main.R"), "result <- helper_fn(41)\n").unwrap();

        let args = base_args(tmp.path());
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            diags
                .iter()
                .any(|(_, d)| d.message == "helper_fn is not defined"),
            "NAMESPACE-only root (no DESCRIPTION) must NOT activate package mode: \
             the cross-file helper must read as undefined in script mode. \
             Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn base_priority_attached_package_exports_resolve_in_check() {
        // `grid` is a base-priority package shipped with R but not attached by
        // default. `raven check` must still resolve its exports after an
        // explicit `library(grid)` call; otherwise package tests that use
        // grid helpers like `grid.ls()` produce false undefined-variable
        // diagnostics.
        let Some(_) = crate::r_subprocess::RSubprocess::new(None) else {
            return;
        };
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.R"), "library(grid)\ngrid.ls()\n").unwrap();

        let args = base_args(tmp.path());
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message == "grid.ls is not defined"),
            "`library(grid)` must make `grid.ls` available to `raven check`. Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn package_own_sysdata_objects_resolve_without_data_raw() {
        // A fetched/committed package source can ship a binary `R/sysdata.rda`
        // with no `data-raw/` generating script at all (e.g. r-lib/cli). The
        // AST scan then finds nothing, so `raven check` must fall back to
        // loading the .rda via the R subprocess — otherwise the package's own
        // R/ code referencing its internal data flags as undefined (issue #429
        // corpus: cli's `emojis`, `spinners`, readr's `date_symbols`).
        let Some(_) = crate::r_subprocess::RSubprocess::new(None) else {
            eprintln!(
                "skipping package_own_sysdata_objects_resolve_without_data_raw: R not available"
            );
            return;
        };
        let fixture_rda =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sysdata_pkg/R/sysdata.rda");
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::copy(&fixture_rda, tmp.path().join("R/sysdata.rda")).unwrap();
        // A real fetched package always ships a DESCRIPTION; `Auto` package mode
        // activates only on a DESCRIPTION `Package:` field (see
        // `effective_workspace`), so without this the directory is not a package
        // workspace and the sysdata contribution never reaches scope.
        fs::write(tmp.path().join("DESCRIPTION"), "Package: sysdatapkg\n").unwrap();
        fs::write(tmp.path().join("NAMESPACE"), "export(get_internal)\n").unwrap();
        fs::write(
            tmp.path().join("R/main.R"),
            "get_internal <- function() sysdata_var1\n",
        )
        .unwrap();

        let args = base_args(tmp.path());
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message == "sysdata_var1 is not defined"),
            "a package's own R/ code must see its R/sysdata.rda objects via the \
             R fallback even with no data-raw/ script. Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn clean_file_exits_ok() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("clean.R"), "x <- 1\ny <- x + 1\n").unwrap();
        assert_eq!(run_blocking(base_args(tmp.path())), EXIT_OK);
    }

    /// Issue #476 (bug B): on a case-insensitive filesystem (macOS/Windows) a
    /// `source("child.r")` whose on-disk entry is `child.R` must still resolve the
    /// child's symbols — the resolved edge target URI is case-corrected to match
    /// the workspace-index key. Gated on actual FS case-insensitivity so it is a
    /// no-op (and not a false failure) on case-sensitive filesystems, where the
    /// two names are genuinely different files.
    #[test]
    fn case_mismatched_source_resolves_on_case_insensitive_fs_476() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("child.R"), "helper <- function() 1\n").unwrap();
        // Detect case-insensitivity: can we open the upper-case file via a
        // lower-case name?
        let case_insensitive = std::fs::metadata(tmp.path().join("child.r")).is_ok();
        if !case_insensitive {
            return; // case-sensitive FS: `child.r` is genuinely missing — skip.
        }
        fs::write(tmp.path().join("main.r"), "source(\"child.r\")\nhelper()\n").unwrap();

        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().join("main.r")];
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message == "helper is not defined"),
            "source(\"child.r\") must resolve on-disk child.R on a case-insensitive FS \
             (issue #476). Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
        // Issue #530: the case-only mismatch now also surfaces a single
        // information-level `source-path-case-mismatch` at the source() call —
        // the portability signal that this breaks on a case-sensitive FS.
        let case_diags: Vec<_> = diags
            .iter()
            .filter(|(_, d)| {
                crate::diagnostic_code::diagnostic_has_code(
                    &d.code,
                    crate::diagnostic_code::SOURCE_PATH_CASE_MISMATCH,
                )
            })
            .collect();
        assert_eq!(
            case_diags.len(),
            1,
            "exactly one source-path-case-mismatch on a case-insensitive FS (issue #530): {:?}",
            case_diags
        );
        assert_eq!(
            case_diags[0].1.severity,
            Some(DiagnosticSeverity::INFORMATION),
            "case-insensitive FS regime is information severity under the default `auto` policy"
        );
        assert_eq!(
            case_diags[0].1.range.start.line, 0,
            "diagnostic must be anchored at the source() call (line 0), not drift elsewhere"
        );
    }

    /// Issue #530: on a case-SENSITIVE filesystem, `source("child.r")` when only
    /// `child.R` exists now RESOLVES the file into the graph (so `helper` is
    /// defined — no cascade of false undefined-variable warnings) and emits a
    /// single warning-level `source-path-case-mismatch` at the source() call.
    /// This deliberately overturns the earlier #476 "stays undefined" behavior:
    /// the cascade that dropped the file was exactly the bug #530 fixes. The
    /// converse (a genuine typo with no case-insensitive match, or an ambiguous
    /// 2+-match) still stays unresolved — covered by the resolver unit tests.
    /// Gated to case-sensitive filesystems; a no-op on macOS/Windows where the
    /// case-insensitive test above covers the information-level variant.
    #[test]
    fn case_mismatched_source_resolves_with_warning_on_case_sensitive_fs_530() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("child.R"), "helper <- function() 1\n").unwrap();
        let case_insensitive = std::fs::metadata(tmp.path().join("child.r")).is_ok();
        if case_insensitive {
            return; // case-insensitive FS: child.r aliases child.R — covered above.
        }
        fs::write(tmp.path().join("main.r"), "source(\"child.r\")\nhelper()\n").unwrap();

        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().join("main.r")];
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message == "helper is not defined"),
            "issue #530: a single case-only match must resolve into the graph on a \
             case-sensitive FS — no undefined-variable cascade. Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
        let case_diags: Vec<_> = diags
            .iter()
            .filter(|(_, d)| {
                crate::diagnostic_code::diagnostic_has_code(
                    &d.code,
                    crate::diagnostic_code::SOURCE_PATH_CASE_MISMATCH,
                )
            })
            .collect();
        assert_eq!(
            case_diags.len(),
            1,
            "exactly one source-path-case-mismatch on a case-sensitive FS (issue #530): {:?}",
            case_diags
        );
        assert_eq!(
            case_diags[0].1.severity,
            Some(DiagnosticSeverity::WARNING),
            "case-sensitive FS regime is warning severity under the default `auto` policy"
        );
        assert_eq!(
            case_diags[0].1.range.start.line, 0,
            "diagnostic must be anchored at the source() call (line 0), not drift elsewhere"
        );
    }

    /// Issue #476 (bug A): the WHOLE diagnostic pipeline must be deterministic, not
    /// just the graph-build order. Runs `collect_diagnostics_blocking` twice over a
    /// hub-and-spoke workspace (each run does an independent rayon scan + graph
    /// build + scope resolution) and asserts byte-identical diagnostic sets — so a
    /// future order-sensitivity introduced downstream of the graph sort (e.g. a
    /// HashMap in scope merging) is caught, not only backward-edge ordering.
    #[test]
    fn whole_pipeline_diagnostics_are_deterministic_476() {
        let tmp = TempDir::new().unwrap();
        // A shared helper hub plus several files that source it and a sibling that
        // (deliberately) references an undefined symbol, to give the run a stable
        // non-empty diagnostic set whose ordering/content could otherwise drift.
        fs::write(tmp.path().join("hub.r"), "shared <- function() 1\n").unwrap();
        for p in ["zeta.r", "alpha.r", "mike.r", "bravo.r"] {
            fs::write(
                tmp.path().join(p),
                "source(\"hub.r\")\nshared()\nnot_defined_here\n",
            )
            .unwrap();
        }

        let collect = || -> Vec<String> {
            let args = base_args(tmp.path());
            let mut v: Vec<String> = collect_diagnostics_blocking(&args)
                .into_iter()
                .map(|(p, d)| {
                    format!(
                        "{}:{}:{}",
                        p.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                        d.range.start.line,
                        d.message
                    )
                })
                .collect();
            v.sort();
            v
        };

        let first = collect();
        let second = collect();
        assert_eq!(
            first, second,
            "whole-pipeline diagnostics must be identical run-to-run (issue #476 bug A)"
        );
        // Sanity: `shared` resolves (no false positive) while the genuine undefined
        // is reported — so the determinism assertion is over a meaningful set.
        assert!(
            !first.iter().any(|d| d.contains("shared is not defined")),
            "shared() should resolve via the hub. Got: {first:?}"
        );
        assert!(
            first
                .iter()
                .any(|d| d.contains("not_defined_here is not defined")),
            "the genuine undefined should be reported. Got: {first:?}"
        );
    }

    /// Issue #476 (bug C): a heavily-sourced hub's parent-prefix over-approximates
    /// the union over ALL its callers, so a symbol the hub itself produces via a
    /// forward `source()` can ALSO land in the hub's parent prefix. The
    /// identical-binding no-op then left that name flagged parent-prefix-only and
    /// the leak filter dropped it when the hub was itself forward-sourced. This
    /// reproduces the `getArray` miniature: `caller` defines `helper` then sources
    /// `hub`; `hub` forward-sources `defs` (which also defines `helper`); `user`
    /// sources `hub` and must see `helper`.
    #[test]
    fn hub_forward_sourced_symbol_resolves_when_also_in_parent_prefix_476() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("defs.r"), "helper <- function() 1\n").unwrap();
        fs::write(tmp.path().join("hub.r"), "source(\"defs.r\")\n").unwrap();
        // A caller that sources `defs.r` itself BEFORE sourcing the hub. This seeds
        // `helper` into the hub's over-approximated parent prefix with the SAME
        // binding (`source_uri = defs.r`, same position) the hub's own forward
        // source produces — so the identical-binding no-op fires and (pre-fix)
        // leaves `helper` marked parent-prefix-only. This is the exact stale-marker
        // path the fix targets; a caller that defined its OWN `helper` would carry a
        // different `source_uri`, take the non-identical merge branch (which already
        // clears the marker), and resolve even without the fix.
        fs::write(
            tmp.path().join("caller.r"),
            "source(\"defs.r\")\nsource(\"hub.r\")\n",
        )
        .unwrap();
        // The file under test sources the hub and uses helper.
        fs::write(tmp.path().join("user.r"), "source(\"hub.r\")\nhelper()\n").unwrap();

        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().join("user.r")];
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message == "helper is not defined"),
            "a hub's genuinely forward-sourced symbol must resolve even when it also \
             appears in the hub's parent prefix (issue #476). Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    /// Issue #476 (bug D): a TOP-LEVEL `source(child, local = TRUE)` evaluates the
    /// child in the caller's environment (`.GlobalEnv` at top level, R `?source`),
    /// so the child sees the parent's prior top-level bindings — including regular
    /// (non-declared) symbols. This reproduces the `getPhrase` miniature: `parent`
    /// sources `defs` (defining `helper`) then sources `use` with `local = TRUE`;
    /// `use` must see `helper`.
    #[test]
    fn top_level_local_true_child_inherits_parent_regular_symbols_476() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("defs.r"), "helper <- function() 1\n").unwrap();
        fs::write(tmp.path().join("use.r"), "helper()\n").unwrap();
        fs::write(
            tmp.path().join("parent.r"),
            "source(\"defs.r\")\nsource(\"use.r\", local = TRUE)\n",
        )
        .unwrap();

        let mut args = base_args(tmp.path());
        args.paths = vec![tmp.path().join("use.r")];
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message == "helper is not defined"),
            "a top-level source(local = TRUE) child must inherit the parent's prior \
             regular symbols (issue #476). Diagnostics: {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    /// Shared setup for the two diagnostics-collection test drivers below.
    /// Mirrors `run`'s indexing + R-init + target-collection prelude and returns
    /// the indexed `WorldState` and report targets. Factored out so the
    /// sequential and parallel drivers resolve from byte-identical state — the
    /// precondition that makes `parallel_collection_matches_sequential` a
    /// like-for-like comparison (drift between two copied preludes would
    /// silently weaken the equivalence assertion). R-independent: callers that
    /// want package awareness configure `additionalLibraryPaths`.
    async fn setup_check_state_and_targets(
        args: &CheckArgs,
    ) -> (crate::state::WorldState, Vec<PathBuf>) {
        let root = std::fs::canonicalize(args.workspace.as_ref().unwrap()).unwrap();
        let workspace_url = Url::from_file_path(&root).unwrap();
        let mut state =
            build_indexed_state(&root, &workspace_url, args.no_config, None, &root).unwrap();
        if !args.report_uninstalled {
            state.cross_file_config.packages_report_generic_uninstalled = false;
        }
        maybe_init_r(&mut state, &root).await;
        state.resolve_system_file_in_workspace_cli_compat(None);
        maybe_load_sysdata_fallback(&mut state).await;
        let mut operator_error = false;
        let targets = collect_report_targets_with_exclusions(
            &args.paths,
            &root,
            &state.workspace_exclusions,
            &mut operator_error,
        );
        prefetch_reported_packages(&state, &targets).await;
        (state, targets)
    }

    /// Run `raven check` and capture the diagnostics it would compute, without
    /// the process-global stdout capture the renderer uses. Mirrors `run`'s
    /// indexing + report loop so a test can assert on the exact `(path,
    /// Diagnostic)` pairs (line/character) rather than just the exit code.
    fn collect_diagnostics_blocking(args: &CheckArgs) -> Vec<(PathBuf, Diagnostic)> {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut state, targets) = setup_check_state_and_targets(args).await;
            let mut all = Vec::new();
            for path in &targets {
                let uri = Url::from_file_path(path).unwrap();
                if let Some(entry) = state.workspace_index.get(&uri) {
                    let doc = crate::state::document_from_workspace_entry(
                        &uri,
                        &entry,
                        crate::chunks::classify_chunk_document(uri.path()),
                    );
                    state.documents.open_prepared(
                        uri.clone(),
                        doc,
                        entry.metadata.clone(),
                        entry.artifacts.clone(),
                        None,
                    );
                } else {
                    let text = crate::state::read_source(path).unwrap();
                    // Same disk-fallback path `run` uses, so production and test
                    // exercise identical behavior (including the backward-directive
                    // parent_content map).
                    super::open_disk_fallback_target(&mut state, &uri, path, &text);
                }
                let diags = compute_file_diagnostics(&mut state, &uri).await;
                state.close_document(&uri);
                for d in diags {
                    all.push((path.clone(), d));
                }
            }
            all
        })
    }

    #[test]
    fn check_discovers_stan_and_reports_syntax_and_undefined_variables() {
        let tmp = TempDir::new().unwrap();
        let invalid = tmp.path().join("invalid.stan");
        let valid = tmp.path().join("valid.StAn");
        let semantic = tmp.path().join("semantic.stan");
        fs::write(&invalid, "data { int N }\nmodel {}\n").unwrap();
        fs::write(
            &valid,
            "parameters { real x; } model { x ~ normal(0, 1); }\n",
        )
        .unwrap();
        fs::write(&semantic, "model { target += unknown_value; }\n").unwrap();
        fs::write(
            tmp.path().join("helper.stanfunctions"),
            "functions { real helper(real x) { return x; } }\n",
        )
        .unwrap();

        let args = model_diagnostic_args(tmp.path(), "stan = \"on\"\n");
        let findings = collect_diagnostics_blocking(&args);
        assert!(!findings.is_empty());
        assert_eq!(findings.len(), 2, "{findings:#?}");
        assert!(findings.iter().any(|(path, diagnostic)| {
            path.ends_with("invalid.stan")
                && diagnostic.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "syntax-error".to_string(),
                    ))
        }));
        assert!(findings.iter().any(|(path, diagnostic)| {
            path.ends_with("semantic.stan")
                && diagnostic.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "undefined-variable".to_string(),
                    ))
                && diagnostic.message == "unknown_value is not defined"
        }));
        assert_eq!(run_blocking(args), EXIT_LINT_FAILED);
    }

    #[test]
    fn check_discovers_all_jags_dialect_extensions_case_insensitively() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("invalid.JAGS"), "model { x <- * 1 }\n").unwrap();
        fs::write(tmp.path().join("invalid.BUGS"), "model { y <- * 1 }\n").unwrap();
        fs::write(tmp.path().join("invalid.BUG"), "model { z <- * 1 }\n").unwrap();
        fs::write(tmp.path().join("valid.jags"), "model { x <- 1 }\n").unwrap();
        fs::write(
            tmp.path().join("semantic-only.bugs"),
            "model { x <- unknown_identifier + 1 }\n",
        )
        .unwrap();
        fs::write(tmp.path().join("near-miss.bugx"), "model { w <- * 1 }\n").unwrap();

        let args = model_diagnostic_args(tmp.path(), "jags = \"on\"\n");
        let findings = collect_diagnostics_blocking(&args);
        assert!(!findings.is_empty());
        assert!(findings.iter().all(|(path, diagnostic)| {
            (path.ends_with("invalid.JAGS")
                || path.ends_with("invalid.BUGS")
                || path.ends_with("invalid.BUG"))
                && diagnostic.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "syntax-error".to_string(),
                    ))
        }));
        assert_eq!(run_blocking(args), EXIT_LINT_FAILED);
    }

    #[test]
    fn explicit_mixed_case_jags_uses_jags_disk_fallback() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("model.JaGs");
        fs::write(&model, "model { x <- * 1 }\n").unwrap();
        let mut args = model_diagnostic_args(tmp.path(), "jags = \"on\"\n");
        args.paths = vec![model.clone()];
        let findings = collect_diagnostics_blocking(&args);
        assert!(!findings.is_empty());
        let expected = model.canonicalize().unwrap();
        assert!(
            findings
                .iter()
                .all(|(path, _)| path.canonicalize().unwrap() == expected)
        );
    }

    #[test]
    fn explicit_mixed_case_bugs_uses_jags_disk_fallback() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("model.BuGs");
        fs::write(&model, "model { x <- * 1 }\n").unwrap();
        let mut args = model_diagnostic_args(tmp.path(), "jags = \"on\"\n");
        args.paths = vec![model.clone()];
        let findings = collect_diagnostics_blocking(&args);
        assert!(!findings.is_empty());
        let expected = model.canonicalize().unwrap();
        assert!(
            findings
                .iter()
                .all(|(path, _)| path.canonicalize().unwrap() == expected)
        );
    }

    #[test]
    fn explicit_mixed_case_bug_uses_jags_disk_fallback() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("model.BuG");
        fs::write(&model, "model { x <- * 1 }\n").unwrap();
        let mut args = model_diagnostic_args(tmp.path(), "jags = \"on\"\n");
        args.paths = vec![model.clone()];
        let findings = collect_diagnostics_blocking(&args);
        assert!(!findings.is_empty());
        let expected = model.canonicalize().unwrap();
        assert!(
            findings
                .iter()
                .all(|(path, _)| path.canonicalize().unwrap() == expected)
        );
    }

    #[test]
    fn explicit_bug_near_miss_is_not_a_check_target() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("unsupported.bugx");
        fs::write(&model, "model { x <- * 1 }\n").unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![model];
        assert!(collect_diagnostics_blocking(&args).is_empty());
        assert_eq!(run_blocking(args), EXIT_OK);
    }

    #[test]
    fn jags_dialect_disk_fallbacks_strip_utf8_bom() {
        let tmp = TempDir::new().unwrap();
        for name in ["valid.jags", "valid.BUGS", "valid.BUG"] {
            let mut bytes = vec![0xef, 0xbb, 0xbf];
            bytes.extend_from_slice(b"model { x <- 1 }\n");
            fs::write(tmp.path().join(name), bytes).unwrap();
        }
        let args = model_diagnostic_args(tmp.path(), "jags = \"on\"\n");
        assert!(collect_diagnostics_blocking(&args).is_empty());
        assert_eq!(run_blocking(args), EXIT_OK);
    }

    #[test]
    fn explicit_mixed_case_stan_uses_stan_disk_fallback() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("model.STAN");
        fs::write(&model, "model { target += ; }\n").unwrap();
        let mut args = model_diagnostic_args(tmp.path(), "stan = \"on\"\n");
        args.paths = vec![model.clone()];
        let findings = collect_diagnostics_blocking(&args);
        assert!(!findings.is_empty());
        let expected = model.canonicalize().unwrap();
        assert!(
            findings
                .iter()
                .all(|(path, _)| path.canonicalize().unwrap() == expected)
        );
    }

    #[test]
    fn stan_raven_path_directive_does_not_run_r_missing_file_phase() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("model.stan");
        fs::write(&model, "# raven: source missing.R\nmodel {}\n").unwrap();
        let mut args = model_diagnostic_args(tmp.path(), "stan = \"on\"\n");
        args.paths = vec![model];
        assert!(collect_diagnostics_blocking(&args).is_empty());
        assert_eq!(run_blocking(args), EXIT_OK);
    }

    #[test]
    fn contextual_cli_materialization_keeps_stan_analysis_inert() {
        let tmp = TempDir::new().unwrap();
        let model = tmp.path().join("contextual.stan");
        fs::write(&model, "data { int N }\nmodel {}\n").unwrap();
        let uri = Url::from_file_path(&model).unwrap();
        let workspace_root = Url::from_directory_path(tmp.path()).unwrap();
        let execution = crate::cross_file::tar_source::ContextualProviderExecution {
            uri: uri.clone(),
            context: crate::cross_file::path_resolve::PathContext::forward_without_metadata(
                &uri,
                Some(&workspace_root),
            )
            .unwrap(),
            prefer_supplied_path_context: true,
            contextual: true,
        };
        let mut state = crate::state::WorldState::new();
        state.workspace_scan_complete = true;
        state.workspace_folders = vec![workspace_root.clone()];
        state.cross_file_config.stan_diagnostics_enabled = true;
        assert!(materialize_cli_contextual_provider(
            &mut state,
            &execution,
            Some(&workspace_root)
        ));

        let document = {
            let entry = state.workspace_index.get(&uri).unwrap();
            assert_eq!(entry.tree.as_ref().unwrap().root_node().kind(), "program");
            assert!(entry.metadata.sources.is_empty());
            assert!(entry.metadata.working_directory.is_none());
            assert!(entry.metadata.inherited_working_directory.is_none());
            assert!(entry.artifacts.exported_interface.is_empty());
            assert!(entry.loaded_packages.is_empty());
            assert!(entry.data_packages.is_empty());
            crate::state::document_from_workspace_entry(
                &uri,
                &entry,
                state.chunk_kind_for_closed_file(&uri),
            )
        };
        state.documents.insert(uri.clone(), document);
        assert!(
            !crate::handlers::diagnostics(&state, &uri, &crate::handlers::DiagCancelToken::never())
                .is_empty()
        );
    }

    /// Like `collect_diagnostics_blocking` but drives the REAL parallel
    /// collection path (`collect_target_diagnostics`, the rayon phase `run`
    /// uses), so a test can assert the parallel output equals the sequential
    /// reference. Shares `setup_check_state_and_targets` with the sequential
    /// driver so the two start from identical state.
    fn collect_diagnostics_parallel_blocking(args: &CheckArgs) -> Vec<(PathBuf, Diagnostic)> {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut state, targets) = setup_check_state_and_targets(args).await;
            let (all, _packages, _oe) = collect_target_diagnostics(&mut state, &targets).await;
            all
        })
    }

    /// Issue #479 WI3: the parallel per-file collection must be byte-identical to
    /// a sequential run. The fixture is a hub sourced by several spokes with
    /// cross-file `source()` references, so neighbor artifacts are resolved —
    /// exactly the data a cross-worker open-document leak would corrupt. The
    /// sequential reference opens one doc at a time into `state.documents`; the
    /// parallel path uses per-worker one-entry overlays.
    #[test]
    fn parallel_collection_matches_sequential() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("hub.r"), "shared <- function() 1\n").unwrap();
        for p in ["a.r", "b.r", "c.r", "d.r"] {
            fs::write(
                tmp.path().join(p),
                "source(\"hub.r\")\nshared()\nbad_symbol\n",
            )
            .unwrap();
        }
        let args = base_args(tmp.path());

        let normalize = |mut v: Vec<(PathBuf, Diagnostic)>| {
            v.sort_by(|(pa, da), (pb, db)| {
                pa.cmp(pb)
                    .then(da.range.start.line.cmp(&db.range.start.line))
                    .then(da.range.start.character.cmp(&db.range.start.character))
                    .then(da.message.cmp(&db.message))
            });
            v.into_iter()
                .map(|(p, d)| (p, d.range.start.line, d.range.start.character, d.message))
                .collect::<Vec<_>>()
        };

        let seq = normalize(collect_diagnostics_blocking(&args));
        let par = normalize(collect_diagnostics_parallel_blocking(&args));
        assert_eq!(
            par, seq,
            "parallel collection must be byte-identical to the sequential reference"
        );
        // Sanity: the fixture actually exercises cross-file resolution — each
        // spoke flags `bad_symbol` (undefined), while `shared()` resolves through
        // the sourced hub and is NOT flagged.
        assert!(
            seq.iter().any(|(_, _, _, m)| m.contains("bad_symbol")),
            "fixture should produce undefined-variable diagnostics: {seq:?}"
        );
        assert!(
            !seq.iter().any(|(_, _, _, m)| m.contains("shared")),
            "shared() should resolve cross-file and not be flagged: {seq:?}"
        );
    }

    #[test]
    fn raven_check_tar_source_resolves_member_at_command_boundary() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("R")).unwrap();
        fs::write(
            workspace.path().join("R/functions.R"),
            "helper_fn <- function() 1\n",
        )
        .unwrap();
        let target = workspace.path().join("_targets.R");
        fs::write(&target, "targets::tar_source(\"R\")\nhelper_fn()\n").unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        assert_eq!(
            run_blocking(args.clone()),
            EXIT_OK,
            "the real check command boundary must consume finalized tar artifacts"
        );
        let diagnostics = collect_diagnostics_blocking(&args);
        assert!(
            !diagnostics
                .iter()
                .any(|(_, diagnostic)| diagnostic.message == "helper_fn is not defined"),
            "tar member symbols must be visible to the report: {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_single_file_shiny_loads_helpers_but_not_global() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("R")).unwrap();
        fs::write(workspace.path().join("R/helper.R"), "helper_value <- 1\n").unwrap();
        fs::write(workspace.path().join("global.R"), "global_only <- 1\n").unwrap();
        let target = workspace.path().join("app.R");
        fs::write(&target, "helper_value\nglobal_only\n").unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        let diagnostics = collect_diagnostics_blocking(&args);
        assert!(
            !diagnostics
                .iter()
                .any(|(_, diagnostic)| diagnostic.message == "helper_value is not defined"),
            "single-file helpers must be visible: {diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|(_, diagnostic)| diagnostic.message == "global_only is not defined"),
            "single-file mode must not implicitly load adjacent global.R: {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_legacy_shiny_wins_and_keeps_entries_isolated() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("R")).unwrap();
        fs::write(workspace.path().join("R/helper.R"), "helper_value <- 1\n").unwrap();
        fs::write(workspace.path().join("global.R"), "global_value <- 1\n").unwrap();
        fs::write(workspace.path().join("ui.R"), "ui_only <- 1\n").unwrap();
        fs::write(workspace.path().join("app.R"), "app_only <- 1\n").unwrap();
        let target = workspace.path().join("server.R");
        fs::write(&target, "helper_value\nglobal_value\nui_only\napp_only\n").unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        let diagnostics = collect_diagnostics_blocking(&args);
        for visible in ["helper_value", "global_value"] {
            assert!(
                !diagnostics.iter().any(
                    |(_, diagnostic)| diagnostic.message == format!("{visible} is not defined")
                ),
                "legacy support/global binding {visible} must be visible: {diagnostics:?}"
            );
        }
        for isolated in ["ui_only", "app_only"] {
            assert!(
                diagnostics
                    .iter()
                    .any(|(_, diagnostic)| diagnostic.message
                        == format!("{isolated} is not defined")),
                "legacy mode must not borrow {isolated}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn raven_check_shiny_disable_marker_suppresses_helper_batch() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("R")).unwrap();
        fs::write(workspace.path().join("R/helper.R"), "helper_value <- 1\n").unwrap();
        fs::write(workspace.path().join("R/_DISABLE_AUTOLOAD.r"), "").unwrap();
        let target = workspace.path().join("app.R");
        fs::write(&target, "helper_value\n").unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        let diagnostics = collect_diagnostics_blocking(&args);
        assert!(
            diagnostics
                .iter()
                .any(|(_, diagnostic)| diagnostic.message == "helper_value is not defined"),
            "the disable marker must remove implicit helper scope: {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_shiny_exclusions_apply_before_layout_selection() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("R")).unwrap();
        fs::write(
            workspace.path().join("raven.toml"),
            "[workspace]\nexclude = [\"server.R\", \"R/_disable_autoload.R\", \"R/excluded.R\"]\n",
        )
        .unwrap();
        fs::write(workspace.path().join("server.R"), "legacy_only <- 1\n").unwrap();
        fs::write(workspace.path().join("global.R"), "global_only <- 1\n").unwrap();
        fs::write(workspace.path().join("R/_disable_autoload.R"), "").unwrap();
        fs::write(workspace.path().join("R/included.R"), "included <- 1\n").unwrap();
        fs::write(workspace.path().join("R/excluded.R"), "excluded <- 1\n").unwrap();
        let target = workspace.path().join("app.R");
        fs::write(&target, "included\nexcluded\nglobal_only\nlegacy_only\n").unwrap();

        let mut args = base_args(workspace.path());
        args.no_config = false;
        args.paths = vec![target.clone()];
        let (state, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(setup_check_state_and_targets(&args));
        let target_uri = Url::from_file_path(target.canonicalize().unwrap()).unwrap();
        assert!(
            !state.workspace_exclusions.is_excluded_uri(&target_uri),
            "app itself must remain included: {:?}",
            state.workspace_exclusions.patterns()
        );
        let server_uri =
            Url::from_file_path(workspace.path().join("server.R").canonicalize().unwrap()).unwrap();
        assert!(
            state.workspace_exclusions.is_excluded_uri(&server_uri),
            "server exclusion must compile: {:?}",
            state.workspace_exclusions.patterns()
        );
        let metadata = state
            .workspace_index
            .get_metadata(&target_uri)
            .expect("app metadata is indexed");
        assert!(
            metadata.sources.iter().any(|source| {
                source.source_batch_kind == Some(crate::cross_file::SourceBatchKind::Shiny)
                    && source.path.ends_with("included.R")
            }),
            "included helper must remain in finalized metadata: shiny={:?}, sources={:?}",
            metadata.shiny_application,
            metadata.sources
        );
        let diagnostics = collect_diagnostics_blocking(&args);
        assert!(
            !diagnostics
                .iter()
                .any(|(_, diagnostic)| diagnostic.message == "included is not defined"),
            "an excluded marker must not suppress included helpers: {diagnostics:?}"
        );
        for absent in ["excluded", "global_only", "legacy_only"] {
            assert!(
                diagnostics
                    .iter()
                    .any(|(_, diagnostic)| diagnostic.message == format!("{absent} is not defined")),
                "excluded server/helper selection must leave {absent} unavailable: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn raven_check_materializes_external_tar_provider_in_consolidated_index() {
        let workspace = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        fs::create_dir_all(external.path().join("R")).unwrap();
        fs::write(
            external.path().join("R/functions.R"),
            "external_helper <- function() 1\n",
        )
        .unwrap();
        let target = external.path().join("_targets.R");
        fs::write(&target, "targets::tar_source(\"R\")\nexternal_helper()\n").unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        let diagnostics = collect_diagnostics_parallel_blocking(&args);
        assert!(
            !diagnostics
                .iter()
                .any(|(_, diagnostic)| diagnostic.message == "external_helper is not defined"),
            "a disk-fallback target must materialize its external provider before diagnostics: \
             {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_repeated_tar_child_preserves_each_occurrence_path_context() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("runtime")).unwrap();
        fs::write(workspace.path().join("child.R"), "source(\"config.R\")\n").unwrap();
        fs::write(
            workspace.path().join("runtime/config.R"),
            "runtime_symbol <- function() 1\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("config.R"),
            "root_symbol <- function() 1\n",
        )
        .unwrap();
        let target = workspace.path().join("_targets.R");
        fs::write(
            &target,
            "# raven: cd runtime\n\
             targets::tar_source(\"../child.R\", change_directory = FALSE)\n\
             targets::tar_source(\"../child.R\", change_directory = TRUE)\n\
             runtime_symbol()\n\
             root_symbol()\n",
        )
        .unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        let diagnostics = collect_diagnostics_parallel_blocking(&args);
        assert!(
            !diagnostics.iter().any(|(_, diagnostic)| matches!(
                diagnostic.message.as_str(),
                "runtime_symbol is not defined" | "root_symbol is not defined"
            )),
            "both executions of one child must resolve nested sources against their own \
             effective PathContext: {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_discovers_repeated_tar_context_below_ordinary_source() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("runtime")).unwrap();
        fs::write(
            workspace.path().join("main.R"),
            "source(\"driver.R\")\nruntime_symbol()\nroot_symbol()\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("driver.R"),
            "# raven: cd runtime\n\
             targets::tar_source(\"../child.R\", change_directory = FALSE)\n\
             targets::tar_source(\"../child.R\", change_directory = TRUE)\n",
        )
        .unwrap();
        fs::write(workspace.path().join("child.R"), "source(\"config.R\")\n").unwrap();
        fs::write(
            workspace.path().join("runtime/config.R"),
            "runtime_symbol <- function() 1\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("config.R"),
            "root_symbol <- function() 1\n",
        )
        .unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![workspace.path().join("main.R")];
        let diagnostics = collect_diagnostics_parallel_blocking(&args);
        assert!(
            !diagnostics.iter().any(|(_, diagnostic)| matches!(
                diagnostic.message.as_str(),
                "runtime_symbol is not defined" | "root_symbol is not defined"
            )),
            "graph-prefix traversal must discover the downstream tar batch: {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_graph_prefix_uses_uri_global_edge_before_tar_batch() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("wd1")).unwrap();
        fs::create_dir_all(workspace.path().join("wd2")).unwrap();
        fs::write(
            workspace.path().join("00_other.R"),
            "# raven: cd wd1\nsource(\"../a.R\", chdir = FALSE)\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("zz_main.R"),
            "# raven: cd wd2\n\
             source(\"../a.R\", chdir = FALSE)\n\
             wd1_symbol()\n\
             root_symbol()\n",
        )
        .unwrap();
        fs::write(workspace.path().join("a.R"), "source(\"driver.R\")\n").unwrap();
        fs::write(
            workspace.path().join("driver.R"),
            "# raven: cd wd1\n\
             targets::tar_source(\"../child.R\", change_directory = FALSE)\n\
             targets::tar_source(\"../child.R\", change_directory = TRUE)\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("wd2/driver.R"),
            "# lexical alternate with no tar batch\n",
        )
        .unwrap();
        fs::write(workspace.path().join("child.R"), "source(\"config.R\")\n").unwrap();
        fs::write(
            workspace.path().join("wd1/config.R"),
            "wd1_symbol <- function() 1\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("config.R"),
            "root_symbol <- function() 1\n",
        )
        .unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![workspace.path().join("zz_main.R")];
        let diagnostics = collect_diagnostics_parallel_blocking(&args);
        assert!(
            !diagnostics.iter().any(|(_, diagnostic)| matches!(
                diagnostic.message.as_str(),
                "wd1_symbol is not defined" | "root_symbol is not defined"
            )),
            "the planner must follow scope's URI-global a.R -> driver.R edge, not the queried \
             caller's lexical wd2/driver.R alternate: {diagnostics:?}"
        );
    }

    #[test]
    fn raven_check_repeated_tar_context_survives_two_ordinary_source_hops() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("runtime")).unwrap();
        fs::create_dir_all(workspace.path().join("shared")).unwrap();
        fs::create_dir_all(workspace.path().join("common")).unwrap();
        fs::write(
            workspace.path().join("shared/child.R"),
            "source(\"../common/helper.R\")\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("common/helper.R"),
            "source(\"config.R\")\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("runtime/config.R"),
            "runtime_deep <- function() 1\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("shared/config.R"),
            "shared_deep <- function() 1\n",
        )
        .unwrap();
        let target = workspace.path().join("_targets.R");
        fs::write(
            &target,
            "# raven: cd runtime\n\
             targets::tar_source(\"../shared/child.R\", change_directory = FALSE)\n\
             targets::tar_source(\"../shared/child.R\", change_directory = TRUE)\n\
             runtime_deep()\n\
             shared_deep()\n",
        )
        .unwrap();

        let mut args = base_args(workspace.path());
        args.paths = vec![target];
        let diagnostics = collect_diagnostics_parallel_blocking(&args);
        assert!(
            !diagnostics.iter().any(|(_, diagnostic)| matches!(
                diagnostic.message.as_str(),
                "runtime_deep is not defined" | "shared_deep is not defined"
            )),
            "the occurrence PathContext must survive child -> helper -> config: {diagnostics:?}"
        );
    }

    #[test]
    fn explicit_rmd_chunk_syntax_error_exits_failed() {
        // A syntax error inside an R chunk of a `.Rmd` must be reported, and at
        // DOCUMENT coordinates (not chunk-relative): the masked text preserves
        // line geometry, so the error's line equals its physical line in the
        // file. Prose lines precede the chunk to make the offset non-trivial.
        let tmp = TempDir::new().unwrap();
        let rmd = tmp.path().join("report.Rmd");
        // Lines (0-based):
        //   0: "# Title"
        //   1: ""
        //   2: "Some prose."
        //   3: "```{r}"
        //   4: "f <- function( {"   <- hard syntax error here
        //   5: "```"
        fs::write(
            &rmd,
            "# Title\n\nSome prose.\n```{r}\nf <- function( {\n```\n",
        )
        .unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![rmd.clone()];
        assert_eq!(run_blocking(args.clone()), EXIT_LINT_FAILED);

        // Assert the finding lands on the chunk body line (document line 4),
        // proving geometry-preserving masking.
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            diags.iter().any(|(_, d)| d.range.start.line == 4
                && d.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)),
            "syntax error expected at document line 4 (0-based); got {:?}",
            diags
                .iter()
                .map(|(_, d)| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// R-free coverage that the CLI warming wiring extends the warm set with a
    /// document's `data(..., package = "pkg")` packages (issue #429). Deleting
    /// either `doc.data_packages` extend in `reported_packages_to_warm`
    /// regresses this. Builds the workspace index (no R subprocess) and inspects
    /// the pure package-set computation directly.
    fn warm_set_for(root: &Path) -> std::collections::HashSet<String> {
        let canon = std::fs::canonicalize(root).unwrap();
        let workspace_url = Url::from_file_path(&canon).unwrap();
        let state = build_indexed_state(&canon, &workspace_url, true, None, &canon).unwrap();
        let mut operator_error = false;
        let targets = collect_report_targets(&[], &canon, &mut operator_error);
        reported_packages_to_warm(&state, &targets)
    }

    #[test]
    fn warming_includes_data_package_from_indexed_r_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("analysis.R"),
            "data(api, package = \"survey\")\n",
        )
        .unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("survey"),
            "data(api, package = \"survey\") in an indexed R file must contribute \
             `survey` to the warm set: {warm:?}"
        );
    }

    #[test]
    fn warming_includes_box_package_from_indexed_r_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("analysis.R"),
            "box::use(syntheticBoxPackage)\n",
        )
        .unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("syntheticBoxPackage"),
            "static box package imports must contribute to the CLI warm set: {warm:?}"
        );
    }

    #[test]
    fn targeted_warming_traverses_indexed_selective_modules() {
        let temp = TempDir::new().unwrap();
        let importer = temp.path().join("main.R");
        fs::write(&importer, "import::here('module.R', value)\n").unwrap();
        fs::write(
            temp.path().join("module.R"),
            "import::here(syntheticImportPackage, value)\n",
        )
        .unwrap();

        let canonical_root = std::fs::canonicalize(temp.path()).unwrap();
        let workspace_url = Url::from_file_path(&canonical_root).unwrap();
        let state =
            build_indexed_state(&canonical_root, &workspace_url, true, None, &canonical_root)
                .unwrap();
        let targets = vec![std::fs::canonicalize(importer).unwrap()];
        let warm = reported_packages_to_warm(&state, &targets);
        assert!(
            warm.contains("syntheticImportPackage"),
            "a package referenced only inside an indexed local module must warm: {warm:?}"
        );
    }

    #[test]
    fn targeted_warming_traverses_selective_modules_from_chunk_file() {
        let temp = TempDir::new().unwrap();
        let report = temp.path().join("report.Rmd");
        fs::write(
            &report,
            "# Report\n\n```{r}\nimport::here(value, .from = \"module.R\")\n```\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("module.R"),
            "import::here(syntheticChunkImportPackage, value)\n",
        )
        .unwrap();

        let canonical_root = std::fs::canonicalize(temp.path()).unwrap();
        let workspace_url = Url::from_file_path(&canonical_root).unwrap();
        let state =
            build_indexed_state(&canonical_root, &workspace_url, true, None, &canonical_root)
                .unwrap();
        let targets = vec![std::fs::canonicalize(report).unwrap()];
        let warm = reported_packages_to_warm(&state, &targets);
        assert!(
            warm.contains("syntheticChunkImportPackage"),
            "a package referenced only inside a local module imported by an Rmd chunk must warm: \
             {warm:?}"
        );
    }

    #[test]
    fn warming_includes_conditional_p_load_target_from_indexed_r_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("analysis.R"),
            "library(pacman)\np_load(syntheticPackage)\nexported_value\n",
        )
        .unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("syntheticPackage"),
            "conditional p_load targets must be warmed without making them semantically active: \
             {warm:?}"
        );
    }

    #[test]
    fn warming_includes_rprofile_p_load_target() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "library(pacman)\np_load(profileSyntheticPackage)\n",
        )
        .unwrap();
        fs::write(tmp.path().join("analysis.R"), "profile_export\n").unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("profileSyntheticPackage"),
            "Rprofile attachment contributions must participate in permissive CLI warming: \
             {warm:?}"
        );
    }

    #[test]
    fn conditional_p_load_reporting_remains_active_only() {
        let tmp = TempDir::new().unwrap();
        let active_path = tmp.path().join("active.R");
        let inactive_path = tmp.path().join("inactive.R");
        fs::write(
            &active_path,
            "library(pacman)\np_load(activeSyntheticPackage)\np_load(\"../bad\")\n",
        )
        .unwrap();
        fs::write(&inactive_path, "p_load(inactiveSyntheticPackage)\n").unwrap();
        let canon = std::fs::canonicalize(tmp.path()).unwrap();
        let workspace_url = Url::from_file_path(&canon).unwrap();
        let state = build_indexed_state(&canon, &workspace_url, true, None, &canon).unwrap();
        let active_uri = Url::from_file_path(std::fs::canonicalize(active_path).unwrap()).unwrap();
        let inactive_uri =
            Url::from_file_path(std::fs::canonicalize(inactive_path).unwrap()).unwrap();
        assert_eq!(
            active_conditional_packages_for_uri(&state, &active_uri),
            vec!["activeSyntheticPackage"]
        );
        assert!(
            active_conditional_packages_for_uri(&state, &inactive_uri).is_empty(),
            "permissive warming must not make an inactive conditional package report as loaded"
        );
    }

    #[test]
    fn workspace_root_selection_is_segment_aware_and_prefers_deepest_root() {
        let tmp = TempDir::new().unwrap();
        let foo = tmp.path().join("foo");
        let foobar = tmp.path().join("foobar");
        let nested = foo.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&foobar).unwrap();
        let foo_uri = Url::from_file_path(&foo).unwrap();
        let foobar_uri = Url::from_file_path(&foobar).unwrap();
        let nested_uri = Url::from_file_path(&nested).unwrap();
        let roots = vec![foo_uri.clone(), foobar_uri.clone(), nested_uri.clone()];

        let nested_file = Url::from_file_path(nested.join("analysis.R")).unwrap();
        assert_eq!(
            deepest_workspace_root(&roots, &nested_file),
            Some(&nested_uri),
            "the most specific containing workspace must win"
        );

        let sibling_file = Url::from_file_path(foobar.join("analysis.R")).unwrap();
        assert_eq!(
            deepest_workspace_root(&roots, &sibling_file),
            Some(&foobar_uri),
            "a sibling whose name shares a raw prefix must not match foo"
        );
    }

    #[test]
    fn warming_includes_data_package_from_chunk_file() {
        // Chunk files are outside the R-only index, so the chunk-file branch of
        // `reported_packages_to_warm` reads them from disk and masks the body.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("report.Rmd"),
            "# Title\n\n```{r}\ndata(lung, package = \"survival\")\n```\n",
        )
        .unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("survival"),
            "data(lung, package = \"survival\") in an Rmd chunk must contribute \
             `survival` to the warm set: {warm:?}"
        );
    }

    #[test]
    fn warming_includes_box_package_from_chunk_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("report.qmd"),
            "# Report\n\n```{r}\nbox::use(chunkBoxPackage)\n```\n",
        )
        .unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("chunkBoxPackage"),
            "static box imports in masked R chunks must be warmed: {warm:?}"
        );
    }

    #[test]
    fn warming_includes_conditional_p_load_target_from_chunk_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("report.qmd"),
            "# Report\n\n```{r}\nlibrary(pacman)\np_load(chunkSyntheticPackage)\n```\n",
        )
        .unwrap();
        let warm = warm_set_for(tmp.path());
        assert!(
            warm.contains("chunkSyntheticPackage"),
            "conditional p_load targets in masked R chunks must be warmed: {warm:?}"
        );
    }

    #[test]
    fn chunk_conditional_p_load_reporting_remains_active_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("report.Rmd");
        fs::write(
            &path,
            "# Report\n\n```{r}\nlibrary(pacman)\np_load(chunkActivePackage)\n```\n",
        )
        .unwrap();
        let canon = std::fs::canonicalize(tmp.path()).unwrap();
        let workspace_url = Url::from_file_path(&canon).unwrap();
        let mut state = build_indexed_state(&canon, &workspace_url, true, None, &canon).unwrap();
        let uri = Url::from_file_path(std::fs::canonicalize(&path).unwrap()).unwrap();
        let text = crate::state::read_source(&path).unwrap();
        open_disk_fallback_target(&mut state, &uri, &path, &text);
        assert_eq!(
            active_conditional_packages_for_uri(&state, &uri),
            vec!["chunkActivePackage"]
        );
    }

    #[test]
    fn rmd_prose_only_exits_ok() {
        // A `.Rmd` with no R chunks — only prose and a python chunk whose body
        // is invalid R — must produce no findings: prose is masked out and the
        // python chunk is not an R chunk, so its body never reaches the R
        // parser.
        let tmp = TempDir::new().unwrap();
        let rmd = tmp.path().join("prose.Rmd");
        fs::write(
            &rmd,
            "# Heading\n\nJust prose, no R here.\n\n```{python}\nthis is ( not valid R\n```\n",
        )
        .unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![rmd.clone()];
        assert_eq!(run_blocking(args.clone()), EXIT_OK);
        assert!(
            collect_diagnostics_blocking(&args).is_empty(),
            "prose-only / non-R-chunk Rmd must yield no findings"
        );
    }

    #[test]
    fn rmd_chunk_source_resolves_cross_file() {
        // A chunk that `source()`s a sibling R file and then calls a function
        // defined there must NOT flag that function as undefined — proving the
        // CLI wires the opened Rmd's `source()` edge into the dependency graph
        // (the did_open parity step). The reverse — the same call WITHOUT the
        // source() line — IS flagged, so the test can't pass vacuously.
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(
            tmp.path().join("R/util.R"),
            "helper_fn <- function(x) x + 1\n",
        )
        .unwrap();

        // With source(): clean.
        let analysis = tmp.path().join("analysis.Rmd");
        fs::write(
            &analysis,
            "# Analysis\n```{r}\nsource(\"R/util.R\")\nresult <- helper_fn(41)\n```\n",
        )
        .unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![analysis.clone()];
        let with_source = collect_diagnostics_blocking(&args);
        assert!(
            !with_source
                .iter()
                .any(|(_, d)| d.message.contains("helper_fn")),
            "helper_fn must resolve through the sourced sibling; got {:?}",
            with_source
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );

        // Without source(): helper_fn is undefined and flagged. (Guards against
        // a vacuous pass where nothing is checked at all.)
        let no_source = tmp.path().join("nosrc.Rmd");
        fs::write(
            &no_source,
            "# Analysis\n```{r}\nresult <- helper_fn(41)\n```\n",
        )
        .unwrap();
        let mut args2 = base_args(tmp.path());
        args2.paths = vec![no_source.clone()];
        let without_source = collect_diagnostics_blocking(&args2);
        assert!(
            without_source
                .iter()
                .any(|(_, d)| d.message.contains("helper_fn is not defined")),
            "without source(), helper_fn must be flagged undefined; got {:?}",
            without_source
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rmd_params_respected_in_check() {
        // knitr/Quarto inject a `params` object into parameterized reports
        // whose frontmatter declares a top-level `params:` key. `raven check`
        // shares the snapshot diagnostic pipeline, so it must NOT flag `params`
        // as undefined for such a report. The use is a bare assignment RHS (not
        // a call argument) so the undefined-variable collector inspects it.
        let tmp = TempDir::new().unwrap();
        let rmd = tmp.path().join("report.Rmd");
        fs::write(
            &rmd,
            "---\ntitle: Report\nparams:\n  year: 2024\n---\n\n```{r}\nyr <- params$year\n```\n",
        )
        .unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![rmd.clone()];
        assert_eq!(run_blocking(args.clone()), EXIT_OK);

        let diags = collect_diagnostics_blocking(&args);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message.contains("params is not defined")),
            "params must not be flagged when frontmatter declares it; got {:?}",
            diags
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );

        // Guard against a vacuous pass: the SAME chunk without `params:` in the
        // frontmatter MUST flag `params` as undefined.
        let rmd2 = tmp.path().join("noparams.Rmd");
        fs::write(
            &rmd2,
            "---\ntitle: Report\n---\n\n```{r}\nyr <- params$year\n```\n",
        )
        .unwrap();
        let mut args2 = base_args(tmp.path());
        args2.paths = vec![rmd2.clone()];
        let diags2 = collect_diagnostics_blocking(&args2);
        assert!(
            diags2
                .iter()
                .any(|(_, d)| d.message.contains("params is not defined")),
            "without a params: frontmatter, params must be flagged; got {:?}",
            diags2
                .iter()
                .map(|(_, d)| d.message.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rmd_missing_source_in_chunk_flagged() {
        // A chunk that source()s an absent file produces a missing-file
        // diagnostic (WARNING by default) at the chunk's document line.
        // Mirrors `missing_source_passes_when_threshold_raised`: it fails at the
        // default threshold and passes once --max-severity is raised to warning.
        let tmp = TempDir::new().unwrap();
        let rmd = tmp.path().join("main.Rmd");
        // Lines: 0 "# M", 1 "```{r}", 2 "source(\"nope.R\")", 3 "```"
        fs::write(&rmd, "# M\n```{r}\nsource(\"nope.R\")\n```\n").unwrap();
        let mut args = base_args(tmp.path());
        args.paths = vec![rmd.clone()];
        assert_eq!(run_blocking(args.clone()), EXIT_LINT_FAILED);

        // The missing-file finding lands on the chunk body line (document line 2).
        let diags = collect_diagnostics_blocking(&args);
        assert!(
            diags
                .iter()
                .any(|(_, d)| d.range.start.line == 2 && d.message.to_lowercase().contains("not")),
            "missing-source finding expected at document line 2; got {:?}",
            diags
                .iter()
                .map(|(_, d)| (d.range.start.line, d.message.clone()))
                .collect::<Vec<_>>()
        );

        // Raising the threshold to warning lets the WARNING-level missing-file
        // diagnostic pass, exactly like the plain-R case.
        let mut raised = args.clone();
        raised.max_severity = SeverityLevel::Warning;
        assert_eq!(run_blocking(raised), EXIT_OK);
    }

    #[test]
    fn syntax_error_exits_failed() {
        let tmp = TempDir::new().unwrap();
        // Unbalanced paren — a hard syntax error, always reported at ERROR.
        fs::write(tmp.path().join("broken.R"), "f <- function( {\n").unwrap();
        assert_eq!(run_blocking(base_args(tmp.path())), EXIT_LINT_FAILED);
    }

    /// Issue #433 acceptance criteria on the `raven check` surface: the
    /// tidy-eval wrapper repro produces no undefined-variable diagnostic,
    /// while a bare-symbol argument to a non-forwarding function still fails.
    ///
    /// The unqualified `filter` (no `library(dplyr)`) is deliberate — it is
    /// the issue's repro verbatim, and it pins the callee-blind embrace
    /// design: a bare `filter` resolves standard-eval through the builtin
    /// registry, so a covered-verb-position requirement would regress this.
    #[test]
    fn wrapper_mask_forwarding_passes_check() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("wrapper.R"),
            "df <- data.frame(x = 1:5)\n\
             my_filter <- function(data, cond) filter(data, {{ cond }})\n\
             my_filter(df, x > 2)\n",
        )
        .unwrap();
        assert_eq!(run_blocking(base_args(tmp.path())), EXIT_OK);
    }

    #[test]
    fn non_forwarding_function_argument_fails_check() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("plain.R"),
            "f <- function(x) x\nf(undefined_sym)\n",
        )
        .unwrap();
        assert_eq!(run_blocking(base_args(tmp.path())), EXIT_LINT_FAILED);
    }

    #[test]
    fn reports_undefined_symbol_from_attached_package() {
        // Regression (#8): editor/CI parity. When an installed package is
        // attached (`library(pkg)`), the editor prefetches its exports so a
        // bare call that ISN'T an export is flagged undefined. `raven check`
        // must do the same; without warming the cache the package reads as
        // "pending" and the call is suppressed, so the build would silently
        // pass over a real undefined symbol the editor flags.
        //
        // R-free: a fake installed package (a NAMESPACE with no exportPattern)
        // is parsed statically, and `additionalLibraryPaths` makes the library
        // `Ready` without R.
        let workspace = TempDir::new().unwrap();
        let libdir = TempDir::new().unwrap();
        let pkgdir = libdir.path().join("fakepkg");
        fs::create_dir_all(&pkgdir).unwrap();
        fs::write(
            pkgdir.join("DESCRIPTION"),
            "Package: fakepkg\nVersion: 1.0\n",
        )
        .unwrap();
        // Exports exactly one symbol; `fakepkg_missing` is deliberately absent.
        fs::write(pkgdir.join("NAMESPACE"), "export(real_export)\n").unwrap();
        fs::write(
            workspace.path().join("raven.toml"),
            format!(
                "[packages]\nadditionalLibraryPaths = [\"{}\"]\n",
                libdir.path().display()
            ),
        )
        .unwrap();
        fs::write(
            workspace.path().join("main.R"),
            "library(fakepkg)\nfakepkg_missing()\n",
        )
        .unwrap();

        let args = CheckArgs {
            paths: Vec::new(),
            workspace: Some(workspace.path().to_path_buf()),
            config_path: None,
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Info,
            quiet: true,
            color: ColorChoice::Never,
            report_uninstalled: false,
        };
        assert_eq!(run_blocking(args), EXIT_LINT_FAILED);
    }

    #[test]
    fn namespace_full_import_export_resolves_in_value_position() {
        // Regression: in package mode, NAMESPACE `import(pkg)` whole-package
        // imports (`scope_contribution.full_imports`) must have their exports
        // prefetched just like per-file `library()` attaches. Without the
        // warm-up, call-position uses of an installed-but-uncached full
        // import are suppressed by the "pending" heuristic, but
        // value-position references (default args, bare identifiers) emit
        // false "Undefined variable" diagnostics — an asymmetry the editor
        // does not have once its cache is warm.
        //
        // R-free: a fake installed package (static NAMESPACE parse) made
        // `Ready` via `additionalLibraryPaths`, mirroring
        // `reports_undefined_symbol_from_attached_package`.
        let workspace = TempDir::new().unwrap();
        let libdir = TempDir::new().unwrap();
        let pkgdir = libdir.path().join("fakepkg");
        fs::create_dir_all(&pkgdir).unwrap();
        fs::write(
            pkgdir.join("DESCRIPTION"),
            "Package: fakepkg\nVersion: 1.0\n",
        )
        .unwrap();
        fs::write(pkgdir.join("NAMESPACE"), "export(real_export)\n").unwrap();

        // The workspace itself is a package whose NAMESPACE fully imports
        // fakepkg; R/uses.R references the export in VALUE position only.
        fs::write(
            workspace.path().join("DESCRIPTION"),
            "Package: testpkg\nVersion: 1.0\nImports: fakepkg\n",
        )
        .unwrap();
        fs::write(workspace.path().join("NAMESPACE"), "import(fakepkg)\n").unwrap();
        fs::create_dir_all(workspace.path().join("R")).unwrap();
        fs::write(
            workspace.path().join("R").join("uses.R"),
            "f <- function(x, .p = real_export) {\n  identical(x, real_export)\n}\n",
        )
        .unwrap();
        fs::write(
            workspace.path().join("raven.toml"),
            format!(
                "[packages]\nadditionalLibraryPaths = [\"{}\"]\n",
                libdir.path().display()
            ),
        )
        .unwrap();

        let args = CheckArgs {
            paths: Vec::new(),
            workspace: Some(workspace.path().to_path_buf()),
            config_path: None,
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Info,
            quiet: true,
            color: ColorChoice::Never,
            report_uninstalled: false,
        };
        assert_eq!(
            run_blocking(args),
            EXIT_OK,
            "value-position references to a NAMESPACE full import must resolve \
             once its exports are prefetched"
        );
    }

    #[test]
    fn nonindexed_explicit_file_uses_disk_fallback() {
        // Regression (#3): the report loop reuses the workspace index's parsed
        // Document when present, but a target the scan didn't index — here an
        // explicit file outside the (empty) workspace root — must still be read
        // from disk and reported. A syntax error proves the fallback branch ran.
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let broken = outside.path().join("broken.R");
        fs::write(&broken, "g <- function( {\n").unwrap();
        let mut args = base_args(workspace.path());
        args.paths = vec![broken];
        assert_eq!(run_blocking(args), EXIT_LINT_FAILED);
    }

    #[test]
    fn non_utf8_file_is_reported_not_operator_error() {
        // A Latin-1 / Windows-1252 source file (here a bare 0xA0 non-breaking
        // space) is a property of the user's code, like a syntax error — not an
        // operator error. raven check reports it as a finding (exit 1) instead
        // of aborting the whole run (exit 2). The scan silently skips the
        // undecodable file; the report loop's disk fallback turns the encoding
        // failure into the diagnostic.
        let workspace = TempDir::new().unwrap();
        let mut bytes = b"x <- 1\n".to_vec();
        bytes.push(0xA0); // invalid UTF-8 start byte
        bytes.push(b'\n');
        fs::write(workspace.path().join("latin1.R"), bytes).unwrap();
        assert_eq!(run_blocking(base_args(workspace.path())), EXIT_LINT_FAILED);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_file_is_reported() {
        // Regression (#1): a .R file reachable only through a symlinked
        // directory must have its diagnostics reported, not silently skipped.
        // Before the fix, the report walk skipped the symlink while the
        // indexer followed it, so `raven check` exited clean over a real
        // syntax error the editor would flag.
        let workspace = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        // A hard syntax error lives in a directory OUTSIDE the workspace root,
        // reachable only via a symlink inside the workspace.
        fs::write(external.path().join("broken.R"), "f <- function( {\n").unwrap();
        std::os::unix::fs::symlink(external.path(), workspace.path().join("linked")).unwrap();

        assert_eq!(run_blocking(base_args(workspace.path())), EXIT_LINT_FAILED);
    }

    #[test]
    fn missing_source_file_exits_failed() {
        // Demonstrates a cross-file diagnostic that `raven lint` cannot produce:
        // a `source()` of a file that does not exist (missing-file = WARNING by
        // default, which exceeds the default --max-severity of `info`).
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.R"), "source(\"does_not_exist.R\")\n").unwrap();
        assert_eq!(run_blocking(base_args(tmp.path())), EXIT_LINT_FAILED);
    }

    #[test]
    fn missing_source_passes_when_threshold_raised() {
        // With --max-severity warning, a WARNING-level missing-file diagnostic
        // no longer fails the build.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.R"), "source(\"does_not_exist.R\")\n").unwrap();
        let mut args = base_args(tmp.path());
        args.max_severity = SeverityLevel::Warning;
        assert_eq!(run_blocking(args), EXIT_OK);
    }

    /// Regression: `raven check` must honor `packages.additionalLibraryPaths`
    /// from `raven.toml`, exactly as the language server does via
    /// `backend::rebuild_package_library`. The configured path must end up in
    /// the resulting `PackageLibrary`'s search paths. R-independent: the
    /// additional paths are applied after R discovery, so the assertion holds
    /// whether or not R is installed.
    #[tokio::test]
    async fn maybe_init_r_honors_additional_library_paths() {
        let workspace = TempDir::new().unwrap();
        let extra_lib = TempDir::new().unwrap();
        let mut state = crate::state::WorldState::new();
        state.cross_file_config.packages_additional_library_paths =
            vec![extra_lib.path().to_path_buf()];

        maybe_init_r(&mut state, workspace.path()).await;

        assert!(
            state
                .package_library
                .lib_paths()
                .iter()
                .any(|p| p == extra_lib.path()),
            "check must honor packages.additionalLibraryPaths; got {:?}",
            state.package_library.lib_paths()
        );
    }

    /// Regression: `raven check` must honor `packages.enabled = false`,
    /// matching the language server's `backend::rebuild_package_library`, which
    /// returns an empty library without spawning R when packages are disabled.
    /// With the gate, even a configured additional library path is left
    /// unapplied and the library stays not-ready — so a user who disabled
    /// package awareness in their editor doesn't get package diagnostics in CI.
    /// R-independent: the gate short-circuits before R discovery.
    #[tokio::test]
    async fn maybe_init_r_skips_when_packages_disabled() {
        let workspace = TempDir::new().unwrap();
        let extra_lib = TempDir::new().unwrap();
        let mut state = crate::state::WorldState::new();
        state.cross_file_config.packages_enabled = false;
        state.cross_file_config.packages_additional_library_paths =
            vec![extra_lib.path().to_path_buf()];

        maybe_init_r(&mut state, workspace.path()).await;

        assert!(
            !state.package_library_ready,
            "packages.enabled = false must leave the library not-ready"
        );
        assert!(
            state.package_library.lib_paths().is_empty(),
            "packages.enabled = false must not populate library paths; got {:?}",
            state.package_library.lib_paths()
        );
    }

    /// Regression: `raven check` must KEEP the Tier 2/3-carrying library even on a
    /// degraded status (e.g. R absent in CI), and mark it ready so the offline
    /// package-resolution path this PR adds actually runs. A synthetic Tier 3
    /// package — installable nowhere — must resolve through `maybe_init_r`'s
    /// installed library.
    #[tokio::test]
    async fn maybe_init_r_keeps_provider_library_when_r_absent() {
        use crate::package_db::binary_db::{ShippedDbProvenance, write_shipped_db};
        use crate::package_db::model::PackageRecord;

        let _env = crate::package_db::RAVEN_NAMES_DB_ENV_LOCK.lock().await;
        let pkg = "ravenfakecheckpkg";
        let sym = "ravenfakechecksym";
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("names.db");
        write_shipped_db(
            &db_path,
            &[PackageRecord {
                name: pkg.into(),
                version: "1.0.0".into(),
                exports: vec![sym.into()],
                depends: vec![],
                lazy_data: vec![],
            }],
            ShippedDbProvenance {
                source: "test".into(),
                snapshot_date: "2026-05-30".into(),
                package_count: 1,
                raven_version: "9.9.9".into(),
            },
        )
        .unwrap();

        let workspace = TempDir::new().unwrap();
        let _db_env = crate::package_db::NamesDbEnvGuard::set(&db_path);
        let mut state = crate::state::WorldState::new();
        maybe_init_r(&mut state, workspace.path()).await;

        // The Tier 3 provider survived (library not dropped) and is marked ready,
        // so prefetch + resolution can run even though R is irrelevant here.
        assert!(
            state.package_library_ready,
            "a library carrying Tier 3 providers must be marked ready"
        );
        state
            .package_library
            .prefetch_packages(&[pkg.to_string()])
            .await;
        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages(sym, &[pkg.to_string()]),
            "Tier 3 export must resolve through the check-installed library"
        );
        // The synthetic package is still not "installed" (Tier-1-only).
        assert!(!state.package_library.package_exists(pkg));
    }

    /// `maybe_init_r` swaps in a freshly built `PackageLibrary`, which starts
    /// with a `None` local-dev overlay. Like every other library-replacing path
    /// (see `refresh_local_dev_overlay`'s doc and backend.rs's four call sites),
    /// it MUST rebuild that overlay afterward. Without it, a `raven check` run on
    /// a package whose in-root script calls `devtools::load_all()` loses sentinel
    /// resolution and flags every package internal as an undefined variable.
    /// R-independent: the overlay is built from the workspace-derived
    /// contribution, not from R.
    #[tokio::test]
    async fn maybe_init_r_preserves_load_all_overlay() {
        use crate::package_library::LOAD_ALL_SENTINEL;

        let workspace = TempDir::new().unwrap();
        let root = workspace.path();
        fs::create_dir(root.join("R")).unwrap();
        fs::write(
            root.join("DESCRIPTION"),
            "Package: testpkg\nVersion: 0.1.0\n",
        )
        .unwrap();
        fs::write(root.join("NAMESPACE"), "export(exported_fn)\n").unwrap();
        // `my_internal` is non-exported, so outside `R/` it is reachable only via
        // the load_all() overlay.
        fs::write(
            root.join("R/internal.R"),
            "my_internal <- function() 1\nexported_fn <- function() my_internal()\n",
        )
        .unwrap();

        let workspace_url = Url::from_file_path(root).unwrap();
        let mut state = build_indexed_state(root, &workspace_url, true, None, root)
            .expect("build_indexed_state");

        // Precondition: build_indexed_state's `apply_package_event(Initial)`
        // populated the overlay, so the internal resolves under the sentinel.
        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages("my_internal", &[LOAD_ALL_SENTINEL.to_string()]),
            "precondition: build_indexed_state should populate the load_all overlay"
        );

        // Swapping in the R-built library must NOT silently drop the overlay.
        maybe_init_r(&mut state, root).await;

        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages("my_internal", &[LOAD_ALL_SENTINEL.to_string()]),
            "maybe_init_r must rebuild the load_all overlay after replacing the package \
             library; otherwise `raven check` flags load_all() internals as undefined"
        );
    }

    // ── .Rprofile prelude acceptance tests ──────────────────────────────────
    //
    // Cases 1-8, 10, 11 from the `.Rprofile` prelude spec.  Case 10 (raven
    // check parity) is inherently satisfied by using `collect_diagnostics_blocking`
    // throughout — this IS the `raven check` pipeline.  Case 9 (live update) is
    // a later task.  Case 3 (library() export resolution) is left as a
    // contribution-level comment below; end-to-end export resolution requires an
    // installed package in the harness, which is unavailable in CI.

    /// True if any diagnostic on any target is an "Undefined variable" for `name`.
    fn has_undefined(diags: &[(PathBuf, Diagnostic)], name: &str) -> bool {
        diags
            .iter()
            .any(|(_, d)| d.message == format!("{name} is not defined"))
    }

    fn args_for(root: &Path, target: &Path) -> CheckArgs {
        let mut a = base_args(root);
        a.paths = vec![target.to_path_buf()];
        a
    }

    // ── Case 1: resolution via .Rprofile source() ───────────────────────────

    #[test]
    fn acceptance_1_resolution_via_rprofile_source() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::create_dir(root.join("R")).unwrap();
        fs::write(
            root.join("R").join("functions.r"),
            "r_bind <- function(...) 1\n",
        )
        .unwrap();
        fs::write(root.join(".Rprofile"), "source(\"R/functions.r\")\n").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        let foo = root.join("scripts").join("foo.R");
        fs::write(&foo, "r_bind(1, 2)\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &foo));
        assert!(
            !has_undefined(&diags, "r_bind"),
            "prelude must resolve r_bind: {:?}",
            diags
        );
    }

    #[test]
    fn rprofile_excluded_sourced_helper_does_not_seed_prelude_scope() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(
            root.join("raven.toml"),
            "[workspace]\nexclude = [\"helpers/**\"]\n[packages]\nenabled = false\n",
        )
        .unwrap();
        fs::create_dir(root.join("helpers")).unwrap();
        fs::write(
            root.join("helpers").join("setup.R"),
            "excluded_helper <- function() 1\n",
        )
        .unwrap();
        fs::write(root.join(".Rprofile"), "source(\"helpers/setup.R\")\n").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        let foo = root.join("scripts").join("foo.R");
        fs::write(&foo, "excluded_helper()\n").unwrap();

        let mut args = args_for(root, &foo);
        args.no_config = false;
        let diags = collect_diagnostics_blocking(&args);

        assert!(
            has_undefined(&diags, "excluded_helper"),
            "excluded .Rprofile helper must not suppress undefined-variable: {:?}",
            diags
        );
    }

    // ── Case 2: resolution via .Rprofile assignment ─────────────────────────

    #[test]
    fn acceptance_2_resolution_via_rprofile_assignment() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(root.join(".Rprofile"), "my_helper <- function() {}\n").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        let foo = root.join("scripts").join("foo.R");
        fs::write(&foo, "my_helper()\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &foo));
        assert!(!has_undefined(&diags, "my_helper"), "{:?}", diags);
    }

    // ── Case 3: library() suppression — contribution-level only ─────────────
    //
    // End-to-end export resolution for `library(stringr)` → `str_to_sentence()`
    // requires `stringr` to be installed in the test environment's R library,
    // which is unavailable in CI.  The deterministic invariant — that `.Rprofile`
    // `library(pkg)` lines are captured and propagated to
    // `rprofile_attached_packages` — is covered by
    // `package_state::rprofile::tests::harvests_attached_packages` (scanner) and
    // `cross_file::scope::package_contribution_tests::rprofile_prelude_injects_into_scripts_in_package_mode`
    // (scope injection: asserts prelude adds attached packages to `inherited_packages`).
    // No end-to-end test is added here to avoid flakiness.

    // ── Case 11: conditional top-level assignment ────────────────────────────

    #[test]
    fn acceptance_11_conditional_top_level_assignment() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(
            root.join(".Rprofile"),
            "if (interactive()) helper <- function() {}\n",
        )
        .unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        let foo = root.join("scripts").join("foo.R");
        fs::write(&foo, "helper()\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &foo));
        assert!(!has_undefined(&diags, "helper"), "{:?}", diags);
    }

    // ── Case 4: package-mode R/ excluded (prelude must NOT apply) ───────────

    #[test]
    fn acceptance_4_package_mode_excludes_r_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(root.join(".Rprofile"), "zz <- 1\n").unwrap();
        fs::create_dir(root.join("R")).unwrap();
        let uses = root.join("R").join("uses_zz.R");
        fs::write(&uses, "f <- function() zz\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &uses));
        assert!(
            has_undefined(&diags, "zz"),
            "namespace R/ must NOT get the prelude: {:?}",
            diags
        );
    }

    // ── Case 5: package-mode tests/ excluded (prelude must NOT apply) ────────

    #[test]
    fn acceptance_5_package_mode_excludes_tests() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(root.join(".Rprofile"), "zz <- 1\n").unwrap();
        fs::create_dir_all(root.join("tests").join("testthat")).unwrap();
        let testf = root.join("tests").join("testthat").join("test-x.R");
        // Bare reference to avoid test_that() diagnostic interference.
        fs::write(&testf, "zz\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &testf));
        assert!(
            has_undefined(&diags, "zz"),
            "tests must NOT get the prelude: {:?}",
            diags
        );
    }

    // ── Case 4b applicability: data-raw/ applies, vignettes/ withholds ───────

    #[test]
    fn acceptance_applicability_data_raw_gets_prelude() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(root.join(".Rprofile"), "zz <- 1\n").unwrap();
        fs::create_dir(root.join("data-raw")).unwrap();
        let prep = root.join("data-raw").join("prep.R");
        fs::write(&prep, "f <- function() zz\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &prep));
        assert!(
            !has_undefined(&diags, "zz"),
            "data-raw/ must get the prelude (dev-only, run from root): {:?}",
            diags
        );
    }

    #[test]
    fn acceptance_applicability_vignettes_withholds_prelude() {
        // vignettes/*.R is classified as a workspace R file by the harness, so
        // the full end-to-end diagnostic path runs and the prelude-withhold
        // assertion is meaningful.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::write(root.join(".Rprofile"), "zz <- 1\n").unwrap();
        fs::create_dir(root.join("vignettes")).unwrap();
        let vig = root.join("vignettes").join("intro.R");
        fs::write(&vig, "f <- function() zz\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &vig));
        assert!(
            has_undefined(&diags, "zz"),
            "vignettes/ must NOT get the prelude (rebuilt under R CMD check): {:?}",
            diags
        );
    }

    // ── Case 6: script-mode R/ included (prelude MUST apply) ─────────────────

    #[test]
    fn acceptance_6_script_mode_includes_r_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // NO DESCRIPTION → script mode.  R/ is ordinary scripts here.
        fs::write(root.join(".Rprofile"), "zz <- 1\n").unwrap();
        fs::create_dir(root.join("R")).unwrap();
        let uses = root.join("R").join("uses_zz.R");
        fs::write(&uses, "f <- function() zz\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &uses));
        assert!(
            !has_undefined(&diags, "zz"),
            "script-mode R/ must get the prelude: {:?}",
            diags
        );
    }

    // ── Case 7: renv no-op ────────────────────────────────────────────────────

    #[test]
    fn acceptance_7_renv_noop() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("renv")).unwrap();
        // `secret_from_renv` exists ONLY in renv/activate.R; if the prelude
        // wrongly harvested that file, referencing it would resolve and the
        // assertion below would fail. Using a name unique to activate.R makes
        // the test actually prove renv is skipped (not merely that some
        // genuinely-undefined name still flags).
        fs::write(
            root.join("renv").join("activate.R"),
            "secret_from_renv <- 1\n",
        )
        .unwrap();
        fs::write(root.join(".Rprofile"), "source(\"renv/activate.R\")\n").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        let foo = root.join("scripts").join("foo.R");
        fs::write(&foo, "secret_from_renv\n").unwrap();
        let diags = collect_diagnostics_blocking(&args_for(root, &foo));
        assert!(
            has_undefined(&diags, "secret_from_renv"),
            "renv must be a no-op (activate.R not harvested): {:?}",
            diags
        );
    }

    // ── Case 8: no fabricated diagnostics ────────────────────────────────────

    #[test]
    fn acceptance_8_no_fabricated_diagnostics() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("DESCRIPTION"), "Package: pkg\n").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        let foo = root.join("scripts").join("foo.R");
        fs::write(&foo, "ok <- 1\nok\n").unwrap();
        // Baseline: no .Rprofile.
        let baseline = collect_diagnostics_blocking(&args_for(root, &foo));
        // Add a .Rprofile; the diagnostic set must not GAIN anything.
        fs::write(root.join(".Rprofile"), "helper <- function() 1\n").unwrap();
        let with_profile = collect_diagnostics_blocking(&args_for(root, &foo));
        assert!(
            with_profile.len() <= baseline.len(),
            "prelude is suppressive-only; must not add diagnostics. baseline={:?} with={:?}",
            baseline,
            with_profile
        );
    }
}
