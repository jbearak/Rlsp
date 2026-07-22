//
// cross_file/source_detect.rs
//
// Detection of source() and sys.source() calls using tree-sitter
// Detection of rm() and remove() calls for scope tracking
// Detection of library(), require(), loadNamespace() calls for package awareness
//

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Tree};

use super::binding::RuntimeFunctionScope;
use super::scope::FunctionScopeInterval;
use super::types::{
    ForwardSource, ListFilesSourceRequest, SourceLocality, TarSourceRequest,
    TargetsPackageDeclaration, byte_offset_to_utf16_column,
};

/// Maximum depth followed by suppressive-only static source-closure scans.
pub(crate) const STATIC_SOURCE_MAX_DEPTH: usize = 64;

/// Maximum distinct files visited by one suppressive-only static source scan.
pub(crate) const STATIC_SOURCE_MAX_FILES: usize = 1000;

/// Static facts consumed together by Rprofile and preamble worklist scans.
///
/// Each instance is built from one parse tree and one shared lazy binding
/// collection. This keeps definition/removal, package, source, and capture-trust
/// decisions aligned while avoiding the former parser-per-helper pipeline.
#[derive(Debug, Default, Clone)]
pub(crate) struct StaticScriptFacts {
    pub(crate) top_level_defs: std::collections::BTreeSet<String>,
    pub(crate) attached_packages: std::collections::BTreeSet<String>,
    #[cfg(test)]
    pub(crate) source_targets: Vec<String>,
    pub(crate) prelude_events: Vec<StaticPreludeEvent>,
    pub(crate) calls_dev_load_all: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum StaticPreludeEvent {
    Attach(LibraryCall),
    Source(ForwardSource),
}

impl StaticScriptFacts {
    pub(crate) fn from_text(text: &str) -> Self {
        let Some(tree) = crate::parser_pool::with_parser(|parser| parser.parse(text, None)) else {
            return Self::default();
        };
        let mut bindings = super::static_path::LazyStaticBindings::new(tree.root_node(), text);
        let (top_level_defs, calls_dev_load_all) =
            super::scope::static_script_definitions_and_load_all(&tree, text, &mut bindings);
        let attaching_calls =
            top_level_attaching_library_calls_with_bindings(&tree, text, &mut bindings);
        let source_events = static_source_events_with_bindings(&tree, text, &mut bindings);
        #[cfg(test)]
        let source_targets = source_events
            .iter()
            .map(|source| source.path.clone())
            .collect();
        let mut prelude_events: Vec<StaticPreludeEvent> = attaching_calls
            .iter()
            .cloned()
            .map(StaticPreludeEvent::Attach)
            .chain(source_events.into_iter().map(StaticPreludeEvent::Source))
            .collect();
        prelude_events.sort_by_key(|event| match event {
            StaticPreludeEvent::Attach(call) => (call.line, call.column),
            StaticPreludeEvent::Source(source) => (source.line, source.column),
        });
        let attached_packages =
            replay_static_attaching_calls(attaching_calls, &std::collections::BTreeSet::new());
        Self {
            top_level_defs,
            attached_packages,
            #[cfg(test)]
            source_targets,
            prelude_events,
            calls_dev_load_all,
        }
    }
}

/// A statically-extracted `system.file(...)` call used as the path argument
/// to `source()`. Contains the string-literal positional parts and the
/// `package = "P"` value needed to resolve the path at analysis time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SystemFileCall {
    /// Positional string-literal parts (joined with `/` to form the relative path).
    pub parts: Vec<String>,
    /// The `package` argument value (must be a string literal).
    pub package: String,
}

/// Detected rm()/remove() call with extracted symbol names
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmCall {
    /// 0-based line of the rm() call
    pub line: u32,
    /// 0-based UTF-16 column
    pub column: u32,
    /// Symbol names to remove
    pub symbols: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct FramedRmCall {
    pub(crate) call: RmCall,
    pub(crate) runtime_function_scope: RuntimeFunctionScope,
    /// A conservative earlier effect anchor used only by the scope timeline when
    /// capture runtime order cannot be represented by source coordinates.
    conservative_effect_position: Option<(u32, u32)>,
}

impl FramedRmCall {
    fn normalize_scope_effect_position(&mut self) {
        if let Some((line, column)) = self.conservative_effect_position {
            self.call.line = line;
            self.call.column = column;
        }
    }
}

/// Detected `exists("name")` call with the statically-extracted name.
///
/// An `exists("name")` call is a runtime existence probe, but a user who writes
/// it is asserting that `name` is a binding they know about. Raven therefore
/// treats it as a variable declaration equivalent to `# raven: var name` (see
/// `compute_artifacts*` in `scope.rs`, which turns each into a
/// `ScopeEvent::Declaration`). Only string-literal names are captured — a
/// non-literal first argument (`exists(varname)`, `exists(paste0(...))`) is not
/// statically determinable and is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistsCall {
    /// The raw symbol name (string-literal contents, without quotes). The
    /// caller re-applies call-site canonicalization (backtick-wrapping a
    /// non-syntactic name) when synthesizing the declared symbol, mirroring the
    /// `# raven: var` path.
    pub name: String,
    /// 0-based line of the `exists()` call.
    pub line: u32,
}

#[derive(Debug)]
pub(crate) struct FramedExistsCall {
    pub(crate) call: ExistsCall,
    pub(crate) runtime_function_scope: RuntimeFunctionScope,
}

#[derive(Debug)]
pub(crate) struct FramedLibraryCall {
    pub(crate) call: LibraryCall,
    pub(crate) runtime_function_scope: RuntimeFunctionScope,
}

/// Detected lexical library/require/loadNamespace call.
///
/// Targets worker packages are stored separately as
/// [`TargetsPackageDeclaration`] because they apply to the whole pipeline,
/// independent of declaration position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryCall {
    /// Package name (if statically determinable)
    pub package: String,
    /// 0-based line of the call
    pub line: u32,
    /// 0-based UTF-16 column of the call end position
    pub column: u32,
    /// Whether this is inside a function scope
    pub function_scope: Option<FunctionScopeInterval>,
    /// Whether the call *attaches* the package to the search path so its exports
    /// become available as bare names: `true` for `library()` / `require()`,
    /// `false` for `loadNamespace()` (which loads the namespace for qualified
    /// `pkg::fn` access only).
    ///
    /// Consumers must require `attaches` only when they gate on a *bare function*
    /// being resolvable — e.g. Shiny deferred-helper recognition
    /// (`push_shiny_deferred_scopes`), where `reactive`/`render*` are exported
    /// functions called by bare name and so need an attach. Surfaces that
    /// dispatch on the object or namespace rather than a bare name (e.g.
    /// data.table's S3 `[.data.table` via `collect_in_play_packages`) are enabled
    /// by a merely-loaded namespace and must *not* gate on `attaches`.
    ///
    /// Defaults to `false` for forward compatibility on deserialize.
    #[serde(default)]
    pub attaches: bool,
    /// Package that must already be attached immediately before this call for
    /// the load effect to occur.
    ///
    /// This is `Some("pacman")` for a bare `p_load(...)` call and `None` for
    /// direct or namespace-qualified loaders. Keeping the condition on the
    /// ordinary package-load event lets graph-aware scope resolution honor
    /// packages inherited through `source()` without making syntax detection
    /// depend on the workspace graph.
    #[serde(default)]
    pub requires_attached: Option<String>,
}

/// Position immediately before a package-loader effect becomes visible.
///
/// Conditional loaders use this point to ask whether their prerequisite was
/// already attached without accidentally observing the loader's own effect.
pub(crate) fn position_before_library_call(call: &LibraryCall) -> (u32, u32) {
    if call.column > 0 {
        (call.line, call.column - 1)
    } else if call.line > 0 {
        (call.line - 1, u32::MAX)
    } else {
        (0, 0)
    }
}

/// A source range in LSP coordinates: 0-based line, **UTF-16** column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// The member (RHS) token of a `pkg::member` / `pkg:::member` reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceMember {
    pub name: String,
    pub range: SourceRange,
}

/// A detected `pkg::member` / `pkg:::member` (or incomplete `pkg::`) reference.
///
/// INVARIANT: every member-diagnostic site MUST check `!internal` before using
/// `member` — `internal: true` is `:::`, which never gets member validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceReference {
    /// Package name, with any string/backtick delimiters stripped.
    pub package: String,
    /// Range of the package (LHS) token.
    pub package_range: SourceRange,
    /// Member (RHS) token; `None` for an incomplete `pkg::` with no RHS.
    pub member: Option<NamespaceMember>,
    /// `true` for `:::` (internal lookup), `false` for `::`.
    pub internal: bool,
    /// Range of the whole `pkg::member` expression.
    pub range: SourceRange,
}

/// Locate all top-level `source()` and `sys.source()` calls in an R syntax tree and extract their static parameters.
///
/// This function traverses the given tree-sitter `Tree` of R source code and collects each `source()`
/// or `sys.source()` call that has a statically determinable file path. For each detected call it
/// records the file path, the end-position line and UTF-16 column, whether the call is `sys.source`,
/// its precise [`SourceLocality`], and a statically recognized `chdir` state. Unshadowed `F`,
/// `.GlobalEnv`, and `globalenv()` are recognized as global-source equivalents; every other
/// explicit value not statically known to be global is `NonInheriting` so an uncertain call cannot
/// lend global symbols. Calls with non-string or otherwise non-determinable file arguments are
/// ignored.
///
/// # Returns
///
/// A `Vec<ForwardSource>` containing one entry per detected `source()`/`sys.source()` call in document
/// order, with fields populated for path, line, column, `is_sys_source`, precise `locality`, and
/// `chdir`.
///
/// # Examples
///
/// ```
/// use raven::cross_file::source_detect::detect_source_calls;
///
/// let mut parser = tree_sitter::Parser::new();
/// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
/// let source = "source('utils.R', local = TRUE)\n";
/// let tree = parser.parse(source, None).unwrap();
/// let sources = detect_source_calls(&tree, source);
/// assert_eq!(sources.len(), 1);
/// assert_eq!(sources[0].path, "utils.R");
/// assert_eq!(sources[0].locality, raven::cross_file::SourceLocality::CurrentFrame);
/// ```
pub fn detect_source_calls(tree: &Tree, content: &str) -> Vec<ForwardSource> {
    let root = tree.root_node();
    let mut bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_source_calls_with_bindings(tree, content, &mut bindings)
}

/// Internal source detector that reuses an artifact build's shared lazy binding
/// cache. Standalone callers use [`detect_source_calls`].
pub(crate) fn detect_source_calls_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<ForwardSource> {
    detect_source_calls_with_bindings_and_frames(tree, content, bindings)
        .into_iter()
        .map(|detected| detected.source)
        .collect()
}

#[derive(Debug)]
pub(crate) struct FramedSource {
    pub(crate) source: ForwardSource,
    pub(crate) runtime_function_scope: RuntimeFunctionScope,
    /// Whether source-coordinate timeline application can safely order this
    /// call's effects. Dependency detection retains the call regardless.
    pub(crate) scope_lending: bool,
}

impl FramedSource {
    pub(crate) fn contributes_to_timeline(&self) -> bool {
        self.scope_lending
    }
}

pub(crate) fn detect_source_calls_with_bindings_and_frames<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<FramedSource> {
    log::trace!("Starting tree-sitter parsing for source() call detection");
    let mut sources = Vec::new();
    visit_node(
        tree.root_node(),
        content,
        bindings,
        CaptureEvaluationFrame::Caller,
        RuntimeFunctionScope::Lexical,
        true,
        &mut sources,
    );
    log::trace!(
        "Completed source() call detection, found {} calls",
        sources.len()
    );
    for detected in &sources {
        let source = &detected.source;
        log::trace!(
            "  Detected source() call: path='{}' at line {} column {} (is_sys_source={}, locality={:?}, chdir={})",
            source.path,
            source.line,
            source.column,
            source.is_sys_source,
            source.locality,
            source.chdir
        );
    }
    sources
}

/// Return statically known `source()` targets that contribute symbols to the
/// surrounding script scope.
///
/// This is the shared filter policy for `.Rprofile` and test-preamble closure
/// scans. It accepts literal or strictly folded computed paths and excludes
/// directives, non-inheriting calls, function-scoped calls, and unresolved
/// paths. Keeping all filters here prevents the two suppressive scans from
/// drifting as source detection evolves.
fn static_source_events_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<ForwardSource> {
    detect_source_calls_with_bindings_and_frames(tree, content, bindings)
        .into_iter()
        .filter(|detected| {
            !detected.source.is_directive
                && detected.scope_lending
                && detected.source.locality == SourceLocality::Global
                && !detected.source.is_function_scoped
                && !detected.source.path.is_empty()
        })
        .map(|detected| detected.source)
        .collect()
}

fn visit_node<'tree, 'text>(
    node: Node<'tree>,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
    evaluation_frame: CaptureEvaluationFrame,
    runtime_function_scope: RuntimeFunctionScope,
    scope_orderable: bool,
    sources: &mut Vec<FramedSource>,
) {
    if node.kind() == "call" {
        if let Some(mut source) = try_parse_source_call(
            node,
            content,
            capture_bindings,
            evaluation_frame,
            runtime_function_scope,
        ) {
            source.guarded_by_file_exists = source_is_guarded_by_matching_file_exists(
                node,
                &source.path,
                content,
                capture_bindings,
                runtime_function_scope,
            );
            sources.push(FramedSource {
                source,
                runtime_function_scope,
                scope_lending: scope_orderable,
            });
        }
        if let Some(capture) = capture_bindings.capturing_call_kind_at(node) {
            let captured_runtime_scope = runtime_function_scope.for_evaluated_capture_part(node);
            let capture_scope_orderable = scope_orderable
                && !super::binding::capture_evaluation_order_has_source_inversion_with_effect(
                    node,
                    content,
                    capture,
                    evaluation_frame,
                    &mut |root, frame| {
                        capture_root_has_global_source_effect(
                            root,
                            content,
                            capture_bindings,
                            frame,
                            captured_runtime_scope,
                        )
                    },
                );
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, relative_frame, _role| {
                    visit_node(
                        evaluated,
                        content,
                        capture_bindings,
                        relative_frame.relative_to(evaluation_frame),
                        captured_runtime_scope,
                        capture_scope_orderable,
                        sources,
                    )
                },
            );
            return;
        }
    }

    let enters_function = node.kind() == "function_definition";
    let child_frame = if enters_function {
        // Closure creation happens in the surrounding capture frame, while
        // formals, defaults, and the body execute later in the function frame.
        CaptureEvaluationFrame::Caller
    } else {
        evaluation_frame
    };
    let child_runtime_scope = if enters_function {
        runtime_function_scope.enter_function()
    } else {
        runtime_function_scope
    };
    for child in node.children(&mut node.walk()) {
        visit_node(
            child,
            content,
            capture_bindings,
            child_frame,
            child_runtime_scope,
            scope_orderable,
            sources,
        );
    }
}

/// Whether a runtime-evaluated capture root contains a modeled source effect
/// that targets the process global environment.
///
/// This follows the same trusted capture boundaries as source detection itself,
/// so quoted syntax stays inert and nested evaluated capture parts keep their
/// composed evaluation frames. Function bodies are deferred and therefore do
/// not affect the current capture timeline.
fn capture_root_has_global_source_effect<'tree, 'text>(
    node: Node<'tree>,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
    evaluation_frame: CaptureEvaluationFrame,
    runtime_function_scope: RuntimeFunctionScope,
) -> bool {
    if node.kind() == "function_definition" {
        return false;
    }

    if node.kind() == "call" {
        if try_parse_source_call(
            node,
            content,
            capture_bindings,
            evaluation_frame,
            runtime_function_scope,
        )
        .is_some_and(|source| source.locality == SourceLocality::Global)
        {
            return true;
        }

        if let Some(capture) = capture_bindings.capturing_call_kind_at(node) {
            let captured_runtime_scope = runtime_function_scope.for_evaluated_capture_part(node);
            let mut found = false;
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, relative_frame, _role| {
                    found |= capture_root_has_global_source_effect(
                        evaluated,
                        content,
                        capture_bindings,
                        relative_frame.relative_to(evaluation_frame),
                        captured_runtime_scope,
                    );
                },
            );
            return found;
        }
    }

    node.children(&mut node.walk()).any(|child| {
        capture_root_has_global_source_effect(
            child,
            content,
            capture_bindings,
            evaluation_frame,
            runtime_function_scope,
        )
    })
}

const SOURCE_FORMALS: [&str; 17] = [
    "file",
    "local",
    "echo",
    "print.eval",
    "exprs",
    "spaced",
    "verbose",
    "prompt.echo",
    "max.deparse.length",
    "width.cutoff",
    "deparseCtrl",
    "chdir",
    "catch.aborts",
    "encoding",
    "continue.echo",
    "skip.echo",
    "keep.source",
];

const SYS_SOURCE_FORMALS: [&str; 6] = [
    "file",
    "envir",
    "chdir",
    "keep.source",
    "keep.parse.data",
    "toplevel.env",
];

fn source_formals(is_sys_source: bool) -> &'static [&'static str] {
    if is_sys_source {
        SYS_SOURCE_FORMALS.as_slice()
    } else {
        SOURCE_FORMALS.as_slice()
    }
}

fn match_source_call_arguments<'tree>(
    args_node: Node<'tree>,
    content: &str,
    is_sys_source: bool,
    mode: CallMatchMode,
) -> Option<Vec<Option<CallActual<'tree>>>> {
    super::binding::match_call_arguments(args_node, content, source_formals(is_sys_source), mode)
}

fn try_parse_source_call<'tree, 'text>(
    node: Node<'tree>,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
    evaluation_frame: CaptureEvaluationFrame,
    runtime_function_scope: RuntimeFunctionScope,
) -> Option<ForwardSource> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    let is_sys_source = match func_text {
        "source" => false,
        "sys.source" => true,
        _ => return None,
    };

    let args_node = node.child_by_field_name("arguments")?;
    let formals = source_formals(is_sys_source);
    let matched_arguments =
        match_source_call_arguments(args_node, content, is_sys_source, CallMatchMode::Strict)?;
    let Some(CallActual::Value(value_node)) = matched_arguments.first().copied().flatten() else {
        return None;
    };

    // Try normal string-literal path first
    let path = extract_string_literal(value_node, content);

    // If no string literal, try system.file() call in the path position
    let system_file = if path.is_none() {
        try_parse_system_file_call(value_node, content)
    } else {
        None
    };

    // If neither, statically fold computed path expressions —
    // file.path()/normalizePath()/single-assignment variables (issue #638).
    let path = if path.is_none() && system_file.is_none() {
        super::static_path::fold_string_expr(value_node, content, bindings.get())
    } else {
        path
    };

    // Need either a path or a system.file() call
    if path.is_none() && system_file.is_none() {
        return None;
    }

    let deferred_use = !super::binding::is_known_immediate_context(node);
    let mut alias_is_unshadowed = |name: &str| {
        !bindings
            .get()
            .named_alias_may_shadow_at(name, node, deferred_use)
    };

    // Preserve the destination class beyond parsing. A proven current-frame
    // value (`TRUE` or trusted `T`) can later compose with a proven Global
    // capture frame; unknown/environment-valued expressions remain
    // non-inheriting even in that frame.
    let locality = if is_sys_source {
        if matched_global_env_binding(&matched_arguments, content, formals, "envir")
            .is_some_and(|binding| binding.is_trusted_by(&mut alias_is_unshadowed))
        {
            SourceLocality::Global
        } else {
            SourceLocality::NonInheriting
        }
    } else {
        let bool_state = classify_matched_bool(
            &matched_arguments,
            content,
            formals,
            "local",
            &mut alias_is_unshadowed,
        );
        let explicit_global_env =
            matched_global_env_binding(&matched_arguments, content, formals, "local")
                .is_some_and(|binding| binding.is_trusted_by(&mut alias_is_unshadowed));
        match bool_state {
            BoolArgument::Omitted | BoolArgument::Known(false) => SourceLocality::Global,
            BoolArgument::Known(true) => SourceLocality::CurrentFrame,
            BoolArgument::Unknown if explicit_global_env => SourceLocality::Global,
            BoolArgument::Unknown => SourceLocality::NonInheriting,
        }
    };
    let locality = locality.relative_to(evaluation_frame);
    let chdir = matches!(
        classify_matched_bool(
            &matched_arguments,
            content,
            formals,
            "chdir",
            &mut alias_is_unshadowed,
        ),
        BoolArgument::Known(true)
    );

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(ForwardSource {
        path: path.unwrap_or_default(),
        line: start.row as u32,
        column,
        is_directive: false,
        locality,
        chdir,
        is_sys_source,
        explicit_line: false,  // AST-detected sources never have explicit line=N
        directive_line: 0,     // Not applicable for AST-detected sources
        user_line_zero: false, // Not applicable for AST-detected sources
        is_function_scoped: runtime_function_scope.is_function_scoped_at(node),
        system_file,
        resolved_uri: None,
        tar_source_ordinal: None,
        source_batch_kind: None,
        guarded_by_file_exists: false,
    })
}

/// Recognize the narrow optional-source idiom
/// `if (file.exists("path")) source("path")`.
///
/// This is deliberately a diagnostic-only proof. The source call remains in
/// metadata and the dependency graph when the target exists. We accept only a
/// direct or singleton-braced consequence, one positional plain string passed
/// to `file.exists`, and exact decoded path equality. An `else` branch is
/// irrelevant to dominance of the consequence and may be present. Broader
/// conditions would require control-flow analysis and therefore fail closed.
fn source_is_guarded_by_matching_file_exists<'tree, 'text>(
    source_call: Node<'tree>,
    source_path: &str,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
    runtime_function_scope: RuntimeFunctionScope,
) -> bool {
    let Some(parent) = source_call.parent() else {
        return false;
    };
    let if_node = if parent.kind() == "if_statement"
        && parent
            .child_by_field_name("consequence")
            .is_some_and(|consequence| consequence.id() == source_call.id())
    {
        parent
    } else if parent.kind() == "braced_expression"
        && singleton_braced_expression(parent, source_call)
        && parent.parent().is_some_and(|candidate| {
            candidate.kind() == "if_statement"
                && candidate
                    .child_by_field_name("consequence")
                    .is_some_and(|consequence| consequence.id() == parent.id())
        })
    {
        parent.parent().expect("parent checked above")
    } else {
        return false;
    };

    if if_node.has_error() {
        return false;
    }
    let Some(condition) = if_node.child_by_field_name("condition") else {
        return false;
    };
    let deferred_use = runtime_function_scope.is_function_scoped_at(condition)
        || !super::binding::is_known_immediate_context(if_node);
    matching_file_exists_path(condition, content, bindings, deferred_use)
        .is_some_and(|guarded_path| guarded_path == source_path)
}

fn singleton_braced_expression(braced: Node<'_>, expected: Node<'_>) -> bool {
    let mut cursor = braced.walk();
    let mut expressions = braced
        .children_by_field_name("body", &mut cursor)
        .filter(|child| child.is_named());
    expressions
        .next()
        .is_some_and(|expression| expression.id() == expected.id())
        && expressions.next().is_none()
}

fn matching_file_exists_path<'tree, 'text>(
    condition: Node<'tree>,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
    deferred_use: bool,
) -> Option<String> {
    if condition.kind() != "call" || condition.has_error() {
        return None;
    }
    let function = condition.child_by_field_name("function")?;
    match node_text(function, content).trim() {
        "base::file.exists" | "base:::file.exists" => {}
        "file.exists" => {
            if bindings
                .get()
                .named_alias_may_shadow_at("file.exists", condition, deferred_use)
            {
                return None;
            }
        }
        _ => return None,
    }

    let arguments = condition.child_by_field_name("arguments")?;
    let mut argument = None;
    let mut cursor = arguments.walk();
    for child in arguments.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if argument.is_some() || child.child_by_field_name("name").is_some() {
            return None;
        }
        argument = Some(child.child_by_field_name("value")?);
    }
    super::binding::extract_plain_string(argument?, content)
}

/// Parse a `system.file(part1, part2, ..., package = "P")` call node.
/// Returns `Some(SystemFileCall)` when:
/// - The callee is `system.file`
/// - All positional arguments are string literals
/// - There is at least one positional string-literal part
/// - There is a named `package = "P"` argument with a string-literal value
fn try_parse_system_file_call(node: Node, content: &str) -> Option<SystemFileCall> {
    if node.kind() != "call" {
        return None;
    }
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);
    if func_text != "system.file" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut package: Option<String> = None;

    let mut arg_cursor = args_node.walk();
    for child in args_node.children(&mut arg_cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if let Some(name_node) = child.child_by_field_name("name") {
            let name = node_text(name_node, content);
            if name == "package" {
                let value_node = child.child_by_field_name("value")?;
                package = extract_string_literal(value_node, content);
                // Non-literal package arg → bail
                package.as_ref()?;
            }
            // lib.loc: accept when the value is a standard-library reference
            // (.Library, .Library.site, .libPaths()) — these resolve to the
            // default search path our resolver already uses. Reject otherwise.
            if name == "lib.loc" {
                let value_node = child.child_by_field_name("value")?;
                if !is_standard_lib_loc(value_node, content) {
                    return None;
                }
            }
            // fsep: the default is "/"; accept that (no-op), reject others.
            if name == "fsep" {
                let value_node = child.child_by_field_name("value")?;
                let text = node_text(value_node, content);
                if text != "\"/\"" && text != "'/'" {
                    return None;
                }
            }
            // Other named args (e.g. mustWork) don't affect path layout
        } else {
            // Positional argument — must be a string literal
            let value_node = child.child_by_field_name("value")?;
            let part = extract_string_literal(value_node, content)?;
            parts.push(part);
        }
    }

    // Must have at least one part and a package
    let package = package?;
    if parts.is_empty() {
        return None;
    }

    Some(SystemFileCall { parts, package })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoolArgument {
    Omitted,
    Known(bool),
    Unknown,
}

use super::binding::{CallActual, CallMatchMode, CaptureEvaluationFrame};

/// Return the authoritative `file` value node for a valid `source()` or
/// `sys.source()` argument list.
///
/// Detection and file-path intellisense both route through the same R-style
/// exact/partial/positional matcher, including its conservative rejection of
/// invalid and ambiguous shapes, so the two surfaces cannot select different
/// path arguments.
#[cfg_attr(not(test), allow(dead_code))] // Shared strict selector used by static-path tests.
pub(crate) fn source_call_file_value_node<'tree>(
    args_node: &Node<'tree>,
    content: &str,
    is_sys_source: bool,
) -> Option<Node<'tree>> {
    source_call_file_value_node_mode(args_node, content, is_sys_source, CallMatchMode::Strict)
}

pub(crate) fn source_call_file_value_node_recovering<'tree>(
    args_node: &Node<'tree>,
    content: &str,
    is_sys_source: bool,
) -> Option<Node<'tree>> {
    source_call_file_value_node_mode(
        args_node,
        content,
        is_sys_source,
        CallMatchMode::RecoverIncomplete,
    )
}

fn source_call_file_value_node_mode<'tree>(
    args_node: &Node<'tree>,
    content: &str,
    is_sys_source: bool,
    mode: CallMatchMode,
) -> Option<Node<'tree>> {
    let matched = match_source_call_arguments(*args_node, content, is_sys_source, mode)?;
    match matched[0]? {
        CallActual::Value(value) => Some(value),
        CallActual::Missing => None,
    }
}

fn classify_matched_bool(
    matched: &[Option<CallActual>],
    content: &str,
    formals: &[&str],
    param_name: &str,
    alias_is_unshadowed: &mut impl FnMut(&str) -> bool,
) -> BoolArgument {
    let Some(index) = formals.iter().position(|formal| *formal == param_name) else {
        return BoolArgument::Unknown;
    };
    let Some(actual) = matched[index] else {
        return BoolArgument::Omitted;
    };
    let CallActual::Value(value_node) = actual else {
        // Both `local` and `chdir` have FALSE defaults. In R an explicit
        // missing actual invokes that default just like omission.
        return BoolArgument::Known(false);
    };
    match node_text(value_node, content) {
        "TRUE" => BoolArgument::Known(true),
        "FALSE" => BoolArgument::Known(false),
        "T" if alias_is_unshadowed("T") => BoolArgument::Known(true),
        "F" if alias_is_unshadowed("F") => BoolArgument::Known(false),
        _ => BoolArgument::Unknown,
    }
}

#[derive(Clone, Copy)]
enum GlobalEnvBinding {
    Bare(&'static str),
    Qualified,
}

impl GlobalEnvBinding {
    fn is_trusted_by(self, alias_is_unshadowed: &mut impl FnMut(&str) -> bool) -> bool {
        match self {
            Self::Bare(name) => alias_is_unshadowed(name),
            Self::Qualified => true,
        }
    }
}

fn matched_global_env_binding(
    matched: &[Option<CallActual>],
    content: &str,
    formals: &[&str],
    param_name: &str,
) -> Option<GlobalEnvBinding> {
    let index = formals.iter().position(|formal| *formal == param_name)?;
    let Some(CallActual::Value(value_node)) = matched[index] else {
        return None;
    };
    match node_text(value_node, content).trim() {
        "globalenv()" => Some(GlobalEnvBinding::Bare("globalenv")),
        ".GlobalEnv" => Some(GlobalEnvBinding::Bare(".GlobalEnv")),
        "base::globalenv()" => Some(GlobalEnvBinding::Qualified),
        _ => None,
    }
}

fn extract_string_literal(node: Node, content: &str) -> Option<String> {
    if node.kind() == "string" {
        let text = node_text(node, content);
        if (text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\''))
        {
            return Some(text[1..text.len() - 1].to_string());
        }
    }
    None
}

/// Returns true when a `lib.loc` value node refers to a standard library path
/// that our resolver already searches (`.Library`, `.Library.site`, or a call
/// to `.libPaths()`), or is `NULL` (identical to omitting `lib.loc`).
fn is_standard_lib_loc(node: Node, content: &str) -> bool {
    let text = node_text(node, content);
    if text == ".Library" || text == ".Library.site" || text == "NULL" {
        return true;
    }
    // .libPaths() — a call node whose function leaf is `.libPaths`
    if node.kind() == "call"
        && let Some(func) = node.child_by_field_name("function")
    {
        return node_text(func, content) == ".libPaths";
    }
    false
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

fn node_start_position_utf16(node: Node, content: &str) -> (u32, u32) {
    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    (
        start.row as u32,
        byte_offset_to_utf16_column(line_text, start.column),
    )
}

/// Detect rm() and remove() calls in R code.
/// Returns calls that should affect scope (excludes those with non-default envir=).
/// Extracts bare symbols from positional args and string-literal symbols from list=.
pub fn detect_rm_calls(tree: &Tree, content: &str) -> Vec<RmCall> {
    let root = tree.root_node();
    let mut capture_bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_rm_calls_with_bindings(tree, content, &mut capture_bindings)
}

/// Internal removal detector that reuses an artifact build's shared lazy binding
/// cache. Standalone callers use [`detect_rm_calls`].
pub(crate) fn detect_rm_calls_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<RmCall> {
    detect_rm_calls_with_bindings_and_frames(tree, content, capture_bindings)
        .into_iter()
        .map(|detected| detected.call)
        .collect()
}

pub(crate) fn detect_rm_calls_with_bindings_for_scope<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<FramedRmCall> {
    let mut calls = detect_rm_calls_with_bindings_and_frames(tree, content, capture_bindings);
    for call in &mut calls {
        call.normalize_scope_effect_position();
    }
    calls
}

fn detect_rm_calls_with_bindings_and_frames<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<FramedRmCall> {
    log::trace!("Starting tree-sitter parsing for rm() call detection");
    let mut rm_calls = Vec::new();
    visit_node_for_rm(
        tree.root_node(),
        content,
        capture_bindings,
        CaptureEvaluationFrame::Caller,
        RuntimeFunctionScope::Lexical,
        None,
        &mut rm_calls,
    );
    log::trace!(
        "Completed rm() call detection, found {} calls",
        rm_calls.len()
    );
    for detected in &rm_calls {
        let call = &detected.call;
        log::trace!(
            "  Detected rm() call at line {} column {} with symbols: {:?}",
            call.line,
            call.column,
            call.symbols
        );
    }
    rm_calls
}

fn visit_node_for_rm(
    node: Node,
    content: &str,
    capture_bindings: &mut super::static_path::LazyStaticBindings,
    evaluation_frame: CaptureEvaluationFrame,
    runtime_function_scope: RuntimeFunctionScope,
    conservative_effect_position: Option<(u32, u32)>,
    rm_calls: &mut Vec<FramedRmCall>,
) {
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call" {
        if let Some(capture) = capture_bindings.capturing_call_kind_at(node) {
            let conservative_effect_position = conservative_effect_position.or_else(|| {
                super::binding::capture_runtime_order_has_source_inversion(node, content, capture)
                    .then(|| node_start_position_utf16(node, content))
            });
            let captured_runtime_scope = runtime_function_scope.for_evaluated_capture_part(node);
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, relative_frame, _role| {
                    visit_node_for_rm(
                        evaluated,
                        content,
                        capture_bindings,
                        relative_frame.relative_to(evaluation_frame),
                        captured_runtime_scope,
                        conservative_effect_position,
                        rm_calls,
                    )
                },
            );
            return;
        }
        let external_global_escape = evaluation_frame == CaptureEvaluationFrame::ExternalOrUnknown
            && node
                .child_by_field_name("arguments")
                .is_some_and(|arguments| {
                    super::binding::arguments_explicitly_target_global(arguments, content)
                })
            && node
                .child_by_field_name("function")
                .is_some_and(|function| {
                    let name = node_text(function, content);
                    !capture_bindings.get().named_alias_may_shadow_at(
                        name,
                        node,
                        !super::binding::is_known_immediate_context(node),
                    )
                });
        if (evaluation_frame.is_caller_or_global() || external_global_escape)
            && let Some(rm_call) = try_parse_rm_call(node, content)
        {
            // Only add if there are symbols to remove.
            if !rm_call.symbols.is_empty() {
                rm_calls.push(FramedRmCall {
                    call: rm_call,
                    runtime_function_scope,
                    conservative_effect_position,
                });
            }
        }
    }

    let enters_function = node.kind() == "function_definition";
    let child_frame = if enters_function {
        CaptureEvaluationFrame::Caller
    } else {
        evaluation_frame
    };
    let child_runtime_scope = if enters_function {
        runtime_function_scope.enter_function()
    } else {
        runtime_function_scope
    };
    for child in node.children(&mut node.walk()) {
        visit_node_for_rm(
            child,
            content,
            capture_bindings,
            child_frame,
            child_runtime_scope,
            conservative_effect_position,
            rm_calls,
        );
    }
}

fn try_parse_rm_call(node: Node, content: &str) -> Option<RmCall> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    // Check if this is rm() or remove()
    if func_text != "rm" && func_text != "remove" {
        return None;
    }
    if call_is_internal_routine_argument(node, content) {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;

    // Skip if arguments contain error or missing nodes
    if args_node.has_error() {
        return None;
    }

    // Check if rm() has a non-default envir= argument
    // If envir= is present and NOT globalenv() or .GlobalEnv, skip this call
    if has_non_default_envir_for_rm(&args_node, content) {
        return None;
    }

    // Extract bare symbol arguments from positional args
    let mut symbols = extract_bare_symbols(&args_node, content);

    // Extract symbols from list= argument (string literals or c() calls)
    let list_symbols = extract_list_symbols(&args_node, content);
    symbols.extend(list_symbols);

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(RmCall {
        line: start.row as u32,
        column,
        symbols,
    })
}

/// True for the routine call nested as `.Internal(<routine>(...))`.
///
/// The nested call head names a hidden C entry point, not an R binding. For
/// rm/remove detection that means `.Internal(remove(list, envir, inherits))`
/// is the implementation of `rm()`/`remove()`, not a user-level removal call.
fn call_is_internal_routine_argument(call_node: Node, content: &str) -> bool {
    if call_node.kind() != "call" {
        return false;
    }
    let Some(arg) = call_node.parent().filter(|node| node.kind() == "argument") else {
        return false;
    };
    if arg.child_by_field_name("name").is_some() {
        return false;
    }
    if arg
        .child_by_field_name("value")
        .is_none_or(|value| value.id() != call_node.id())
    {
        return false;
    }
    let Some(args) = arg.parent().filter(|node| node.kind() == "arguments") else {
        return false;
    };
    if first_positional_arg_value(args).is_none_or(|value| value.id() != call_node.id()) {
        return false;
    }
    let Some(outer_call) = args.parent().filter(|node| node.kind() == "call") else {
        return false;
    };
    outer_call
        .child_by_field_name("function")
        .and_then(|func| callee_leaf_name(func, content))
        == Some(".Internal")
}

/// The leaf callee name of a call's `function` field.
fn callee_leaf_name<'t>(func: Node<'t>, content: &'t str) -> Option<&'t str> {
    match func.kind() {
        "identifier" => Some(node_text(func, content)),
        "namespace_operator" => func
            .child_by_field_name("rhs")
            .map(|rhs| node_text(rhs, content)),
        _ => None,
    }
}

/// The value node of a call's first positional argument, if any.
fn first_positional_arg_value(args: Node) -> Option<Node> {
    let mut cursor = args.walk();
    args.children(&mut cursor).find_map(|child| {
        (child.kind() == "argument" && child.child_by_field_name("name").is_none())
            .then(|| child.child_by_field_name("value"))
            .flatten()
    })
}

/// Check if rm() call has a non-default envir= argument.
/// Returns true if envir= is present and NOT globalenv() or .GlobalEnv.
/// Returns false if envir= is absent (default) or is globalenv()/.GlobalEnv.
fn has_non_default_envir_for_rm(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "envir"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                let value = node_text(value_node, content).trim();
                // Default-equivalent values: globalenv() or .GlobalEnv
                if matches!(value, "globalenv()" | "base::globalenv()" | ".GlobalEnv") {
                    return false;
                }
                // Any other value means non-default
                return true;
            }
        }
    }
    // No envir= argument means default (global environment)
    false
}

/// Extract bare symbol (identifier) arguments from positional args in rm()/remove() calls.
/// Only extracts identifiers from positional arguments (not named arguments).
fn extract_bare_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    let mut cursor = args_node.walk();

    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            // Only process positional arguments (no name)
            if child.child_by_field_name("name").is_none()
                && let Some(value_node) = child.child_by_field_name("value")
            {
                // Only extract if it's an identifier (bare symbol)
                if value_node.kind() == "identifier" {
                    let symbol_name = node_text(value_node, content).to_string();
                    symbols.push(symbol_name);
                }
            }
        }
    }

    symbols
}

/// Extract symbols from the list= argument in rm()/remove() calls.
///
/// Handles:
/// - `list = "name"` (single string literal)
/// - `list = c("a", "b", "c")` (character vector)
///
/// Skips non-literal expressions (variables, function calls other than c()).
fn extract_list_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut cursor = args_node.walk();

    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            // Look for named argument with name "list"
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "list"
                    && let Some(value_node) = child.child_by_field_name("value")
                {
                    return extract_list_value_symbols(value_node, content);
                }
            }
        }
    }

    Vec::new()
}

/// Extract symbols from the value of a list= argument.
/// Handles string literals and c() calls with string arguments.
fn extract_list_value_symbols(value_node: Node, content: &str) -> Vec<String> {
    match value_node.kind() {
        "string" => {
            // rm(list = "x")
            if let Some(s) = extract_string_literal(value_node, content) {
                vec![s]
            } else {
                vec![]
            }
        }
        "call" => {
            // Check if it's a c() call
            if is_c_call(value_node, content) {
                // rm(list = c("x", "y", "z"))
                extract_c_string_args(value_node, content)
            } else {
                // Dynamic expression like ls() - not supported
                vec![]
            }
        }
        _ => {
            // Variable reference or other expression - not supported
            vec![]
        }
    }
}

/// Detect `exists("name")` calls in R code.
///
/// Returns one [`ExistsCall`] per `exists(...)` call whose name argument is a
/// string literal (`exists("x")`, `exists('x')`, or `exists(x = "x")`). Calls
/// with a non-literal name (`exists(varname)`, `exists(paste0(...))`) or no
/// argument are skipped — the name is not statically determinable. Only the
/// bare `exists` callee is matched (not `pkg::exists`), matching the
/// conservative shape of [`detect_rm_calls`].
pub fn detect_exists_calls(tree: &Tree, content: &str) -> Vec<ExistsCall> {
    let root = tree.root_node();
    let mut capture_bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_exists_calls_with_bindings(tree, content, &mut capture_bindings)
        .into_iter()
        .map(|detected| detected.call)
        .collect()
}

/// Internal existence-probe detector that reuses an artifact build's shared
/// lazy binding cache. Standalone callers use [`detect_exists_calls`].
pub(crate) fn detect_exists_calls_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<FramedExistsCall> {
    let mut calls = Vec::new();
    visit_node_for_exists(
        tree.root_node(),
        content,
        capture_bindings,
        CaptureEvaluationFrame::Caller,
        RuntimeFunctionScope::Lexical,
        &mut calls,
    );
    calls
}

fn visit_node_for_exists(
    node: Node,
    content: &str,
    capture_bindings: &mut super::static_path::LazyStaticBindings,
    evaluation_frame: CaptureEvaluationFrame,
    runtime_function_scope: RuntimeFunctionScope,
    calls: &mut Vec<FramedExistsCall>,
) {
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call" {
        if let Some(capture) = capture_bindings.capturing_call_kind_at(node) {
            let captured_runtime_scope = runtime_function_scope.for_evaluated_capture_part(node);
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, relative_frame, _role| {
                    visit_node_for_exists(
                        evaluated,
                        content,
                        capture_bindings,
                        relative_frame.relative_to(evaluation_frame),
                        captured_runtime_scope,
                        calls,
                    )
                },
            );
            return;
        }
        if evaluation_frame.is_caller_or_global()
            && let Some(call) = try_parse_exists_call(node, content)
        {
            calls.push(FramedExistsCall {
                call,
                runtime_function_scope,
            });
        }
    }
    let enters_function = node.kind() == "function_definition";
    let child_frame = if enters_function {
        CaptureEvaluationFrame::Caller
    } else {
        evaluation_frame
    };
    let child_runtime_scope = if enters_function {
        runtime_function_scope.enter_function()
    } else {
        runtime_function_scope
    };
    for child in node.children(&mut node.walk()) {
        visit_node_for_exists(
            child,
            content,
            capture_bindings,
            child_frame,
            child_runtime_scope,
            calls,
        );
    }
}

fn try_parse_exists_call(node: Node, content: &str) -> Option<ExistsCall> {
    let func_node = node.child_by_field_name("function")?;
    // Bare `exists` only — a `pkg::exists` callee is a `namespace_operator`
    // node, and `file.exists` is a different identifier, so both are excluded.
    if func_node.kind() != "identifier" || node_text(func_node, content) != "exists" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;
    if args_node.has_error() {
        return None;
    }

    // The name lives in the `x` formal: a named `x = "..."` argument, else the
    // first positional argument.
    let value_node = named_arg_value(&args_node, content, "x")
        .or_else(|| first_positional_arg_value(args_node))?;
    let name = extract_string_literal(value_node, content)?;
    // `exists("")` / `exists("   ")` name nothing usable; declare nothing. This
    // matches the `# raven: var` directive, which skips an empty-or-whitespace
    // name (`name.trim().is_empty()` in `directive.rs`).
    if name.trim().is_empty() {
        return None;
    }

    Some(ExistsCall {
        name,
        line: node.start_position().row as u32,
    })
}

/// The value node of the named argument `arg_name`, if present.
fn named_arg_value<'t>(args_node: &Node<'t>, content: &str, arg_name: &str) -> Option<Node<'t>> {
    let mut cursor = args_node.walk();
    args_node.children(&mut cursor).find_map(|child| {
        if child.kind() != "argument" {
            return None;
        }
        let name = child.child_by_field_name("name")?;
        if node_text(name, content) == arg_name {
            child.child_by_field_name("value")
        } else {
            None
        }
    })
}

/// Check if a call node is a c() call (character vector constructor).
fn is_c_call(node: Node, content: &str) -> bool {
    if node.kind() != "call" {
        return false;
    }
    if let Some(func_node) = node.child_by_field_name("function") {
        let func_text = node_text(func_node, content);
        return func_text == "c";
    }
    false
}

/// Extracts string literal arguments from a `c()` call node.
///
/// Only string literal arguments are returned; other argument types are ignored.
///
/// # Examples
///
/// ```text
/// // Given a Tree-sitter `Node` for `c("a", "b")`:
/// let strings = extract_c_string_args(node, r#"c("a", "b")"#);
/// assert_eq!(strings, vec!["a".to_string(), "b".to_string()]);
/// ```
fn extract_c_string_args(node: Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();

    if let Some(args_node) = node.child_by_field_name("arguments") {
        let mut cursor = args_node.walk();
        for child in args_node.children(&mut cursor) {
            if child.kind() == "argument"
                && let Some(value_node) = child.child_by_field_name("value")
                && value_node.kind() == "string"
                && let Some(s) = extract_string_literal(value_node, content)
            {
                symbols.push(s);
            }
        }
    }

    symbols
}

// ============================================================================
// Library Call Detection
// ============================================================================

/// Detects lexical package loads in the file: direct
/// `library()`/`require()`/`loadNamespace()` calls, static pacman `p_load()`
/// calls, apply-family calls whose FUN argument is a bare reference to
/// `library` or `require`, and deterministic package-loader `for` loops.
///
/// Targets `tar_option_set(packages = ...)` declarations are deliberately
/// excluded; use [`detect_targets_pipeline_packages`] for that distinct,
/// pipeline-level channel.
///
/// For direct calls, the package must be a bare identifier (`library(dplyr)`)
/// or a string literal (`library("dplyr")`); direct calls with
/// `character.only = TRUE` or a dynamic package argument are skipped.
///
/// For apply-family calls (`sapply`, `lapply`, `vapply`, `mapply`, plus the
/// bare and `purrr::`-qualified `map`/`walk`/`map_chr`/etc.), `character.only =
/// TRUE` is *required* and the X argument must resolve statically to a vector
/// of string literals — either an inline `c("a","b",...)` or a same-file
/// variable assigned exactly once at the top level via `<-`/`=`, or via an
/// eligible bare/base-qualified `assign("name", c(...))` with its default
/// destination. Nested, conditional, destination-qualified, reassigned, or
/// removed bindings cannot supply candidates.
/// Each apply emits one `LibraryCall` per package, all sharing the apply
/// call's end position.
///
/// A package-loader `for` loop is recognized only when its sequence meets the
/// same static-vector policy and its body unconditionally calls
/// `library(iterator, character.only = TRUE)` or `require()` at the body's top
/// level. The emitted calls share the loop's end position, after every package
/// has been attached.
///
/// Qualified `pacman::p_load()` calls are unconditional. Bare `p_load()` calls
/// carry a `requires_attached = Some("pacman")` precondition that graph-aware
/// scope resolution evaluates at the call; this permits a parent file's
/// earlier `library(pacman)` to enable a sourced child's bare helper without
/// making this syntax pass graph-dependent.
///
/// The returned Vec is sorted in document order by `(line, column)`.
///
/// # Examples
///
/// ```
/// use raven::cross_file::source_detect::detect_library_calls;
///
/// let mut parser = tree_sitter::Parser::new();
/// parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
/// let source = "library(targets)\nlibrary(dplyr)";
/// let tree = parser.parse(source, None).unwrap();
/// let calls = detect_library_calls(&tree, source);
/// assert_eq!(calls.len(), 2);
/// assert_eq!(calls[0].package, "targets");
/// assert_eq!(calls[1].package, "dplyr");
/// assert!(calls[1].attaches);
/// ```
pub fn detect_library_calls(tree: &Tree, content: &str) -> Vec<LibraryCall> {
    let root = tree.root_node();
    let mut capture_bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_library_calls_with_bindings(tree, content, &mut capture_bindings)
}

/// Internal package detector that reuses an artifact build's shared lazy binding
/// cache for trusted-capture classification and package-vector resolution.
/// Standalone callers use [`detect_library_calls`].
pub(crate) fn detect_library_calls_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<LibraryCall> {
    detect_library_walk_output(tree, content, capture_bindings)
        .library_calls
        .into_iter()
        .map(|detected| detected.call)
        .collect()
}

pub(crate) fn detect_library_calls_with_bindings_for_scope<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<FramedLibraryCall> {
    detect_library_walk_output(tree, content, capture_bindings)
        .library_calls
        .into_iter()
        .filter_map(|detected| {
            detected.scope_orderable.then_some(FramedLibraryCall {
                call: detected.call,
                runtime_function_scope: detected.runtime_function_scope,
            })
        })
        .collect()
}

fn detect_library_walk_output<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    capture_bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> LibraryWalkOutput {
    log::trace!("Starting tree-sitter parsing for package detection");
    let mut output = LibraryWalkOutput {
        scan_p_load: content.contains("p_load"),
        ..Default::default()
    };
    let root = tree.root_node();
    visit_node_for_library(
        root,
        content,
        capture_bindings,
        false,
        RuntimeFunctionScope::Lexical,
        true,
        &mut output,
    );
    finalize_tar_option_set_candidates(&mut output);
    output
        .library_calls
        .sort_by_key(|detected| (detected.call.line, detected.call.column));
    output
        .targets_pipeline_packages
        .sort_by_key(|declaration| (declaration.line, declaration.column));
    log::trace!(
        "Completed package detection: {} lexical loads, {} targets pipeline packages",
        output.library_calls.len(),
        output.targets_pipeline_packages.len()
    );
    output
}

/// Parse `text` and return the set of packages it *attaches* via a
/// **top-level** `library()` / `require()` call.
///
/// "Attaches" excludes `loadNamespace()` (which loads a namespace for
/// qualified `pkg::fn` access but does not put exports on the search path —
/// see [`LibraryCall::attaches`]). "Top-level" excludes calls nested inside a
/// function body, which do not attach until that function runs. Calls inside a
/// proven captured argument are likewise excluded, while evaluated `bquote()`
/// splices and `substitute()` environment arguments remain visible.
///
/// This models a testthat preamble file (`tests/testthat/helper*.R` /
/// `setup*.R`): testthat sources such files at the top level before any test
/// runs, so a top-level `library(tidyr)` in a helper attaches tidyr for every
/// sibling test file. The top-level-only gate parallels
/// [`crate::roxygen::extract_top_level_defs`] (both filter to top level),
/// though the justification differs: `extract_top_level_defs` captures only
/// definitions that exist at source time, whereas this excludes function-body
/// `library()` calls because they don't attach until the function runs.
///
/// `requireNamespace()` is NOT a match (it isn't a `library`/`require`/
/// `loadNamespace` call), so it never attaches here.
///
/// Returns an empty set when the text cannot be parsed.
///
/// # Examples
///
/// ```
/// use raven::cross_file::source_detect::extract_attached_packages;
///
/// let pkgs = extract_attached_packages("library(tidyr)\nrequire(dplyr)\n");
/// assert!(pkgs.contains("tidyr"));
/// assert!(pkgs.contains("dplyr"));
///
/// // loadNamespace does not attach; a nested call does not attach at source time.
/// let none = extract_attached_packages("loadNamespace(\"tidyr\")\nf <- function() library(dplyr)\n");
/// assert!(none.is_empty());
/// ```
pub fn extract_attached_packages(text: &str) -> std::collections::BTreeSet<String> {
    let Some(tree) = crate::parser_pool::with_parser(|parser| parser.parse(text, None)) else {
        return std::collections::BTreeSet::new();
    };
    let mut bindings = super::static_path::LazyStaticBindings::new(tree.root_node(), text);
    extract_attached_packages_with_bindings(&tree, text, &mut bindings)
}

fn extract_attached_packages_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> std::collections::BTreeSet<String> {
    replay_static_attaching_calls(
        top_level_attaching_library_calls_with_bindings(tree, content, bindings),
        &std::collections::BTreeSet::new(),
    )
}

fn top_level_attaching_library_calls_with_bindings<'tree, 'text>(
    tree: &'tree Tree,
    content: &'text str,
    bindings: &mut super::static_path::LazyStaticBindings<'tree, 'text>,
) -> Vec<LibraryCall> {
    let mut output = LibraryWalkOutput {
        scan_p_load: content.contains("p_load"),
        ..Default::default()
    };
    visit_node_for_library(
        tree.root_node(),
        content,
        bindings,
        true,
        RuntimeFunctionScope::Lexical,
        true,
        &mut output,
    );
    output
        .library_calls
        .sort_by_key(|detected| (detected.call.line, detected.call.column));
    output
        .library_calls
        .into_iter()
        .map(|detected| detected.call)
        .collect()
}

fn replay_static_attaching_calls(
    calls: impl IntoIterator<Item = LibraryCall>,
    initial_attached: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let mut attached = initial_attached.clone();
    for call in calls {
        if !call.attaches || call.package.is_empty() {
            continue;
        }
        if call
            .requires_attached
            .as_ref()
            .is_none_or(|required| attached.contains(required))
        {
            attached.insert(call.package);
        }
    }
    attached
}

struct LibraryWalkCall {
    call: LibraryCall,
    runtime_function_scope: RuntimeFunctionScope,
    scope_orderable: bool,
}

#[derive(Default)]
struct LibraryWalkOutput {
    library_calls: Vec<LibraryWalkCall>,
    tar_candidates: Vec<TarOptionSetCandidate>,
    targets_pipeline_packages: Vec<TargetsPackageDeclaration>,
    scan_p_load: bool,
}

impl LibraryWalkOutput {
    fn push_call(
        &mut self,
        call: LibraryCall,
        runtime_function_scope: RuntimeFunctionScope,
        scope_orderable: bool,
    ) {
        self.library_calls.push(LibraryWalkCall {
            call,
            runtime_function_scope,
            scope_orderable,
        });
    }

    fn extend_calls(
        &mut self,
        calls: impl IntoIterator<Item = LibraryCall>,
        runtime_function_scope: RuntimeFunctionScope,
        scope_orderable: bool,
    ) {
        for call in calls {
            self.push_call(call, runtime_function_scope, scope_orderable);
        }
    }
}

/// Recursively traverse an AST subtree and collect statically determinable
/// package loads: direct `library`/`require`/`loadNamespace` calls, apply-family
/// calls whose FUN is `library`/`require`, deterministic package-loader `for`
/// loops, and {targets}
/// `tar_option_set(packages = ...)` candidates.
///
/// When `top_level_only` is true, calls lexically inside a
/// `function_definition` are excluded because they do not attach until the
/// function runs. `inside_fn` is recursive state that latches after entering a
/// function; tree-sitter represents R 4.1+ lambdas as `function_definition`
/// nodes too. Full detection passes `top_level_only = false`, preserving calls
/// inside function bodies. Proven capture wrappers are traversed only through
/// their evaluated controls and splices, with the same state as ordinary child
/// recursion.
///
/// Direct calls push at most one [`LibraryCall`]; apply calls may push one entry
/// per package. `tar_option_set` calls are collected separately so the caller
/// can apply [`finalize_tar_option_set_candidates`] after the walk.
fn visit_node_for_library(
    node: Node,
    content: &str,
    capture_bindings: &mut super::static_path::LazyStaticBindings,
    top_level_only: bool,
    runtime_function_scope: RuntimeFunctionScope,
    scope_orderable: bool,
    output: &mut LibraryWalkOutput,
) {
    // Identifier nodes have no children and cannot be calls.
    if node.kind() == "identifier" {
        return;
    }

    if node.kind() == "for_statement"
        && (!top_level_only || !runtime_function_scope.is_function_scoped_at(node))
    {
        output.extend_calls(
            try_parse_for_library_loop(node, content, capture_bindings),
            runtime_function_scope,
            scope_orderable,
        );
    }

    if node.kind() == "call" {
        if let Some(capture) = capture_bindings.capturing_call_kind_at(node) {
            let capture_scope_orderable = scope_orderable
                && !super::binding::capture_evaluation_order_has_source_inversion(
                    node,
                    content,
                    capture,
                    CaptureEvaluationFrame::Caller,
                );
            let captured_runtime_scope = runtime_function_scope.for_evaluated_capture_part(node);
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, _frame, _role| {
                    visit_node_for_library(
                        evaluated,
                        content,
                        capture_bindings,
                        top_level_only,
                        captured_runtime_scope,
                        capture_scope_orderable,
                        output,
                    );
                },
            );
            return;
        }
        // `p_load()` captures its ordinary `...` package arguments rather
        // than evaluating them. Once the callee is known to be pacman's
        // helper, do not recurse into those arguments and misclassify nested
        // call syntax as eagerly evaluated package loads.
        if output.scan_p_load
            && let Some(parsed) = try_parse_pacman_p_load_call(node, content, capture_bindings)
        {
            if !top_level_only || !runtime_function_scope.is_function_scoped_at(node) {
                output.extend_calls(parsed.calls, runtime_function_scope, scope_orderable);
            }
            if parsed.evaluates_controls {
                visit_p_load_evaluated_arguments(
                    node,
                    content,
                    capture_bindings,
                    top_level_only,
                    runtime_function_scope,
                    scope_orderable,
                    output,
                );
            }
            return;
        }
        if !top_level_only || !runtime_function_scope.is_function_scoped_at(node) {
            if let Some(lib_call) = try_parse_library_call(node, content) {
                output.push_call(lib_call, runtime_function_scope, scope_orderable);
            } else {
                // The apply-family and tar_option_set callee name-sets are
                // disjoint, so at most one of these can match.
                output.extend_calls(
                    try_parse_apply_library_call(node, content, capture_bindings),
                    runtime_function_scope,
                    scope_orderable,
                );
                if !runtime_function_scope.is_function_scoped_at(node) {
                    collect_tar_option_set_candidates(
                        node,
                        content,
                        capture_bindings,
                        &mut output.tar_candidates,
                    );
                }
            }
        }
    }

    let child_runtime_scope = if node.kind() == "function_definition" {
        runtime_function_scope.enter_function()
    } else {
        runtime_function_scope
    };
    for child in node.children(&mut node.walk()) {
        visit_node_for_library(
            child,
            content,
            capture_bindings,
            top_level_only,
            child_runtime_scope,
            scope_orderable,
            output,
        );
    }
}

/// Recognize a deterministic package-loader `for` loop.
///
/// The sequence must be a trusted inline `c()` package vector or a resolvable
/// same-file package-vector binding. The body must contain exactly one
/// top-level `library(iterator, character.only = TRUE)` or equivalent
/// `require()` call. A loader nested in control flow is deliberately rejected,
/// as are loops that can reach `next` before the loader, `break` anywhere in
/// the body, or `return()` from the enclosing function. Emitted
/// attachment events are anchored after the complete loop so packages do not
/// become visible while individual iterations are still running.
fn try_parse_for_library_loop(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Vec<LibraryCall> {
    let Some(variable) = node.child_by_field_name("variable") else {
        return Vec::new();
    };
    let Some(iterator) = super::binding::plain_identifier_name(variable, content) else {
        return Vec::new();
    };
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    if contains_loop_break(body) || contains_loop_return(body, content) {
        return Vec::new();
    }
    let statements: Vec<Node> = if body.kind() == "braced_expression" {
        let mut cursor = body.walk();
        body.named_children(&mut cursor).collect()
    } else {
        vec![body]
    };
    let mut loader_index = None;
    for (index, statement) in statements.iter().copied().enumerate() {
        if is_iterator_library_call(statement, iterator, content, bindings)
            && loader_index.replace(index).is_some()
        {
            return Vec::new();
        }
    }
    let Some(loader_index) = loader_index else {
        return Vec::new();
    };
    if statements[..loader_index]
        .iter()
        .copied()
        .any(contains_loop_skip)
        || statements[..loader_index].iter().copied().any(|statement| {
            super::binding::subtree_may_bind_name(statement, content, iterator)
                || contains_remove_call(statement, content)
        })
    {
        return Vec::new();
    }

    let Some(sequence) = node.child_by_field_name("sequence") else {
        return Vec::new();
    };
    let packages = match sequence.kind() {
        "identifier" => {
            let Some(name) = super::binding::plain_identifier_name(sequence, content) else {
                return Vec::new();
            };
            let Some(packages) = bindings.resolve_package_vector_before_for(name, node) else {
                return Vec::new();
            };
            packages
        }
        _ => {
            let Some(packages) = extract_c_strings_strict(sequence, content) else {
                return Vec::new();
            };
            if !bindings.package_c_is_trusted_at(node) {
                return Vec::new();
            }
            packages
        }
    };

    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, end.column);
    packages
        .into_iter()
        .map(|package| LibraryCall {
            package,
            line: end.row as u32,
            column,
            function_scope: None,
            attaches: true,
            requires_attached: None,
        })
        .collect()
}

fn is_iterator_library_call(
    node: Node,
    iterator: &str,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> bool {
    if node.kind() != "call" {
        return false;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return false;
    };
    let loader = node_text(function, content);
    if !matches!(loader, "library" | "require") {
        return false;
    }
    let deferred = !super::binding::is_known_immediate_context(node);
    if bindings
        .get()
        .named_local_binding_may_shadow_without_helper_uncertainty(loader, node, deferred)
    {
        return false;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return false;
    };
    if arguments.has_error() {
        return false;
    }

    let mut package = None;
    let mut character_only = None;
    let mut positional = 0usize;
    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            return false;
        };
        match argument
            .child_by_field_name("name")
            .map(|name| node_text(name, content))
        {
            Some("package") => {
                if package.replace(value).is_some() {
                    return false;
                }
            }
            Some("character.only") => {
                if character_only
                    .replace(matches!(node_text(value, content), "TRUE" | "T"))
                    .is_some()
                {
                    return false;
                }
            }
            Some(_) => {}
            None => {
                positional += 1;
                if positional == 1 {
                    if package.replace(value).is_some() {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }
    }
    character_only == Some(true)
        && package.is_some_and(|value| {
            super::binding::plain_identifier_name(value, content) == Some(iterator)
        })
}

fn contains_remove_call(node: Node, content: &str) -> bool {
    if node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| {
                let leaf = function.child_by_field_name("rhs").unwrap_or(function);
                matches!(
                    super::binding::plain_identifier_name(leaf, content),
                    Some("rm" | "remove")
                )
            })
    {
        return true;
    }
    if node.kind() == "function_definition" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| contains_remove_call(child, content))
}

fn contains_loop_skip(node: Node) -> bool {
    if matches!(node.kind(), "next" | "break") {
        return true;
    }
    if node.kind() == "function_definition" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(contains_loop_skip)
}

fn contains_loop_break(node: Node) -> bool {
    if node.kind() == "break" {
        return true;
    }
    if node.kind() == "function_definition" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(contains_loop_break)
}

fn contains_loop_return(node: Node, content: &str) -> bool {
    if node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|function| is_base_return_function(function, content))
    {
        return true;
    }
    if node.kind() == "function_definition" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| contains_loop_return(child, content))
}

fn is_base_return_function(function: Node, content: &str) -> bool {
    let is_plain_name = |node: Node, expected: &str| {
        super::binding::plain_identifier_name(node, content) == Some(expected)
            || super::binding::extract_plain_string(node, content).as_deref() == Some(expected)
    };
    if is_plain_name(function, "return") {
        return true;
    }
    if function.kind() != "namespace_operator" {
        return false;
    }
    let Some(lhs) = function.child_by_field_name("lhs") else {
        return false;
    };
    let Some(rhs) = function.child_by_field_name("rhs") else {
        return false;
    };
    is_plain_name(lhs, "base") && is_plain_name(rhs, "return")
}

fn visit_p_load_evaluated_arguments(
    node: Node,
    content: &str,
    capture_bindings: &mut super::static_path::LazyStaticBindings,
    top_level_only: bool,
    runtime_function_scope: RuntimeFunctionScope,
    scope_orderable: bool,
    output: &mut LibraryWalkOutput,
) {
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        let Some(name) = argument.child_by_field_name("name") else {
            continue;
        };
        if !matches!(
            node_text(name, content),
            "char" | "character.only" | "install" | "update"
        ) {
            continue;
        }
        if let Some(value) = argument.child_by_field_name("value") {
            visit_node_for_library(
                value,
                content,
                capture_bindings,
                top_level_only,
                runtime_function_scope,
                scope_orderable,
                output,
            );
        }
    }
}

/// Parse a call AST node and, if it is a static `library()`, `require()`, or `loadNamespace()` invocation,
/// return a `LibraryCall` containing the package name and the byte position (line and UTF-16 column) at the end of the call.
///
/// Returns `Some(LibraryCall)` when:
///
/// - the function name is `library`, `require`, or `loadNamespace`,
/// - the call's arguments are syntactically valid,
/// - the package name is statically determinable from a bare identifier or string literal (including a named `package=` argument),
/// - `character.only = TRUE` is not present (such calls are skipped as dynamic).
///
/// Returns `None` otherwise.
///
/// # Examples
///
/// Given a tree-sitter `Node` for a call and the source `content`,
/// `try_parse_library_call` returns a `LibraryCall` when the package is statically known.
/// (Constructing a Node requires a tree-sitter parse; this example is illustrative.)
fn try_parse_library_call(node: Node, content: &str) -> Option<LibraryCall> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    // Check if this is library(), require(), or loadNamespace()
    if func_text != "library" && func_text != "require" && func_text != "loadNamespace" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;

    // Skip if arguments contain error or missing nodes
    if args_node.has_error() {
        return None;
    }

    // Check for character.only = TRUE - skip these calls (dynamic package name)
    if has_character_only_true(&args_node, content) {
        return None;
    }

    // Extract package name from first argument
    let package = extract_package_name(&args_node, content)?;

    // Get position at the end of the call (after the closing paren)
    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, end.column);

    Some(LibraryCall {
        package,
        line: end.row as u32,
        column,
        // function_scope will be populated later in task 6.2
        function_scope: None,
        // `library`/`require` attach; `loadNamespace` (the only other name this
        // function admits) merely loads the namespace. Allowlist rather than
        // `!= "loadNamespace"` so a future non-attaching loader admitted above
        // is not silently classified as attaching.
        attaches: func_text == "library" || func_text == "require",
        requires_attached: None,
    })
}

/// Detect `pkg::member` / `pkg:::member` references (and incomplete `pkg::`
/// forms that parse as a `namespace_operator` with no RHS) across the document.
///
/// Canonical source for live LSP namespace metadata. Ranges are LSP UTF-16
/// coordinates. Reuses `namespace_completion::unquote_package` for delimiter
/// stripping (delimiter-strip only, not full R literal unescaping).
pub fn detect_namespace_references(tree: &Tree, content: &str) -> Vec<NamespaceReference> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    visit_node_for_namespace(tree.root_node(), content, &lines, &mut out);
    out.sort_by_key(|r| (r.range.start_line, r.range.start_column));
    out
}

fn visit_node_for_namespace(
    node: Node,
    content: &str,
    lines: &[&str],
    out: &mut Vec<NamespaceReference>,
) {
    if node.kind() == "namespace_operator"
        && let Some(reference) = parse_namespace_operator(node, content, lines)
    {
        out.push(reference);
    }
    let mut walk = node.walk();
    for child in node.children(&mut walk) {
        visit_node_for_namespace(child, content, lines, out);
    }
}

fn parse_namespace_operator(ns: Node, content: &str, lines: &[&str]) -> Option<NamespaceReference> {
    let lhs = ns.child_by_field_name("lhs")?;
    // R accepts only an identifier or string/backtick literal as the package
    // qualifier. Anything else (`a$b::x`) is an invalid LHS — ignore it.
    if !matches!(lhs.kind(), "identifier" | "string") {
        return None;
    }
    let package = crate::namespace_completion::unquote_package(node_text(lhs, content));
    if package.is_empty() {
        return None;
    }

    // The operator (`::`/`:::`) is the unnamed child whose text is colons.
    let mut walk = ns.walk();
    let op = ns
        .children(&mut walk)
        .find(|c| matches!(node_text(*c, content), "::" | ":::"))?;
    let internal = node_text(op, content) == ":::";

    let member = ns.child_by_field_name("rhs").and_then(|rhs| {
        if !matches!(rhs.kind(), "identifier" | "string") {
            return None;
        }
        let name = crate::namespace_completion::unquote_package(node_text(rhs, content));
        if name.is_empty() {
            return None;
        }
        Some(NamespaceMember {
            name: name.to_string(),
            range: node_range_utf16(rhs, lines),
        })
    });

    Some(NamespaceReference {
        package: package.to_string(),
        package_range: node_range_utf16(lhs, lines),
        member,
        internal,
        range: node_range_utf16(ns, lines),
    })
}

/// Convert a node's tree-sitter byte-column range into LSP UTF-16 columns.
fn node_range_utf16(node: Node, lines: &[&str]) -> SourceRange {
    let start = node.start_position();
    let end = node.end_position();
    let col = |row: usize, byte_col: usize| -> u32 {
        let line = lines.get(row).copied().unwrap_or("");
        crate::utf16::byte_offset_to_utf16_column(line, byte_col)
    };
    SourceRange {
        start_line: start.row as u32,
        start_column: col(start.row, start.column),
        end_line: end.row as u32,
        end_column: col(end.row, end.column),
    }
}

/// Determine whether an arguments node sets `character.only` to `TRUE` or `T`.
///
/// Scans the children of `args_node` for a named argument `character.only` and
/// returns `true` only if its value text is the literal `TRUE` or `T`.
///
/// # Returns
///
/// `true` if `character.only` is explicitly `TRUE` or `T`, `false` otherwise.
///
/// # Examples
///
/// `args_node` is the Tree-sitter `arguments` node for a call like
/// `library(foo, character.only = TRUE)` and `content` is the source text
/// containing that call. The function will return `true` for that node.
fn has_character_only_true(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "character.only"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                let value = node_text(value_node, content);
                return value == "TRUE" || value == "T";
            }
        }
    }
    false
}

/// Determine a statically determinable package name from a call's argument list.
///
/// Looks for a named `package` argument first; if absent, inspects the first positional argument.
/// Returns `Some(package_name)` when the argument is a bare identifier (e.g., `library(dplyr)`)
/// or a string literal (e.g., `library("dplyr")` or `library('dplyr')`). Returns `None` for variables,
/// expressions, or other dynamic package specifications.
///
/// # Arguments
///
/// * `args_node` - The Tree-sitter `arguments` node of a call expression.
/// * `content` - The source text for extracting string literal contents.
///
/// # Returns
///
/// `Some(String)` containing the package name when statically determinable, `None` otherwise.
///
/// # Examples
///
/// ```text
/// // Parse an R call, obtain its arguments node, then:
/// let pkg = extract_package_name(&args_node, source_text);
/// assert_eq!(pkg, Some("dplyr".to_string()));
/// ```
fn extract_package_name(args_node: &Node, content: &str) -> Option<String> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Look for named "package" argument first
    for child in &children {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
        {
            let name = node_text(name_node, content);
            if name == "package"
                && let Some(value_node) = child.child_by_field_name("value")
            {
                return extract_package_value(value_node, content);
            }
        }
    }

    // Use first positional argument
    for child in &children {
        if child.kind() == "argument"
            && child.child_by_field_name("name").is_none()
            && let Some(value_node) = child.child_by_field_name("value")
        {
            return extract_package_value(value_node, content);
        }
    }

    None
}

/// Extract package name from a value node.
/// Handles bare identifiers (library(dplyr)) and string literals (library("dplyr")).
fn extract_package_value(node: Node, content: &str) -> Option<String> {
    match node.kind() {
        "identifier" => {
            // Bare identifier: library(dplyr)
            Some(node_text(node, content).to_string())
        }
        "string" => {
            // String literal: library("dplyr") or library('dplyr')
            extract_string_literal(node, content)
        }
        _ => {
            // Variable, expression, or other dynamic value - skip
            None
        }
    }
}

// ============================================================================
// Apply-Family Library Detection (issue #172)
// ============================================================================

/// Apply-family functions whose bare-identifier form may load packages
/// dynamically when paired with `library`/`require` and `character.only = TRUE`.
///
/// Restricted to functions with a clean `(X, FUN, ...)` shape (or `(FUN, ...)`
/// for `mapply`). Excluded:
/// - `map2`/`walk2`/`map2_*` — two parallel X vectors with FUN at position 2.
/// - `pmap`/`pwalk` — X is a list of vectors, not a single vector.
/// - `map_if`/`map_at` — `(X, predicate-or-selector, FUN, ...)`, FUN at position 2.
const APPLY_BARE_NAMES: &[&str] = &[
    "sapply", "lapply", "vapply", "mapply", "map", "walk", "imap", "iwalk", "map_chr", "map_int",
    "map_dbl", "map_lgl", "map_raw", "map_dfr", "map_dfc", "map_vec",
];

/// Apply-family functions accepted under the `purrr::` namespace.
const APPLY_PURRR_NAMES: &[&str] = &[
    "map", "walk", "imap", "iwalk", "map_chr", "map_int", "map_dbl", "map_lgl", "map_raw",
    "map_dfr", "map_dfc", "map_vec",
];

/// Return `(x_position, fun_position)` describing where in the call's
/// positional argument list the X (vector) and FUN (mapped function) values
/// are expected, for a supported apply-family function. Returns `None` when
/// the function isn't one we recognise.
///
/// Most apply functions (`sapply`, `lapply`, `vapply`, the purrr `map`/`walk`
/// family) take `(X, FUN, ...)`, so X is at position 0 and FUN at position 1.
/// `mapply`'s signature is `(FUN, ...)`, so FUN is at position 0 and the
/// first vector at position 1.
fn apply_arg_positions(func_node: Node, content: &str) -> Option<(usize, usize)> {
    let name = match func_node.kind() {
        "identifier" => {
            let n = node_text(func_node, content);
            if !APPLY_BARE_NAMES.contains(&n) {
                return None;
            }
            n
        }
        "namespace_operator" => {
            let mut cursor = func_node.walk();
            let named_children: Vec<Node> = func_node
                .children(&mut cursor)
                .filter(|c| c.is_named())
                .collect();
            if named_children.len() != 2 {
                return None;
            }
            if node_text(named_children[0], content) != "purrr" {
                return None;
            }
            let n = node_text(named_children[1], content);
            if !APPLY_PURRR_NAMES.contains(&n) {
                return None;
            }
            n
        }
        _ => return None,
    };
    match name {
        "mapply" => Some((1, 0)),
        _ => Some((0, 1)),
    }
}

/// Return the strings from a strict bare `c()` character vector.
fn extract_c_strings_strict(node: Node, content: &str) -> Option<Vec<String>> {
    super::binding::extract_bare_c_package_strings(node, content)
        .map(|pairs| pairs.into_iter().map(|(string, _)| string).collect())
}

/// Try to interpret `node` as an apply-family call that loads a static vector
/// of packages — e.g. `sapply(c("dplyr","tidyr"), library, character.only = TRUE)`.
///
/// Returns one `LibraryCall` per package when, given the call's
/// `(x_position, fun_position)` from [`apply_arg_positions`]:
/// - `character.only = TRUE` (or `T`) is present,
/// - the positional arg at `fun_position` is the bare identifier `library`
///   or `require` (so we don't match calls like
///   `sapply(c("dplyr"), identity, library, ...)` where library is just a
///   `...`-passthrough),
/// - the positional arg at `x_position` resolves to a static `Vec<String>`
///   via inline `c(...)` or a same-file variable in `bindings`.
///
/// All emitted entries share the apply call's end position.
fn try_parse_apply_library_call(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Vec<LibraryCall> {
    let Some(func_node) = node.child_by_field_name("function") else {
        return Vec::new();
    };
    let Some((x_pos, fun_pos)) = apply_arg_positions(func_node, content) else {
        return Vec::new();
    };

    let Some(args_node) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    if args_node.has_error() {
        return Vec::new();
    }
    if !has_character_only_true(&args_node, content) {
        return Vec::new();
    }

    // Positional argument values, in source order. Named args (including
    // `character.only =`, `simplify =`, `FUN =`, `X =`, etc.) are excluded
    // here — see `test_apply_with_named_x_arg_skipped` for the limitation.
    let mut positional_values: Vec<Node> = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        if child.child_by_field_name("name").is_some() {
            continue;
        }
        if let Some(value) = child.child_by_field_name("value") {
            positional_values.push(value);
        }
    }

    // FUN must sit at the position dictated by the apply's signature and be a
    // bare `library`/`require` identifier — not just appear *somewhere* among
    // the positional args.
    let Some(&fun_value) = positional_values.get(fun_pos) else {
        return Vec::new();
    };
    if fun_value.kind() != "identifier" {
        return Vec::new();
    }
    let fun_text = node_text(fun_value, content);
    if fun_text != "library" && fun_text != "require" {
        return Vec::new();
    }

    let Some(&x_value) = positional_values.get(x_pos) else {
        return Vec::new();
    };
    let packages: Vec<String> = match x_value.kind() {
        "identifier" => {
            let Some(text) = super::binding::plain_identifier_name(x_value, content) else {
                return Vec::new();
            };
            match bindings.resolve_package_vector(text, node) {
                Some(packages) => packages,
                None => return Vec::new(),
            }
        }
        _ => {
            let Some(packages) = extract_c_strings_strict(x_value, content) else {
                return Vec::new();
            };
            if !bindings.package_c_is_trusted_at(node) {
                return Vec::new();
            }
            packages
        }
    };

    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, end.column);
    let line = end.row as u32;

    packages
        .into_iter()
        .map(|package| LibraryCall {
            package,
            line,
            column,
            function_scope: None,
            // This path only fires for `library`/`require` applied via
            // `sapply`/`lapply`/etc. (see the `fun_text` guard above), both of
            // which attach.
            attaches: true,
            requires_attached: None,
        })
        .collect()
}

// ============================================================================
// pacman::p_load Package Detection (issue #660)
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
enum PacmanCallee {
    Qualified,
    Bare,
}

/// Classify `pacman::p_load` / `pacman:::p_load` and the bare `p_load`
/// spelling. Namespace qualification is authoritative; the bare spelling is
/// returned only when Raven's lexical binding model proves that no local
/// binding may shadow pacman's exported helper at this use.
fn pacman_p_load_callee(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Option<PacmanCallee> {
    let function = node.child_by_field_name("function")?;
    match function.kind() {
        "namespace_operator" => {
            let lhs = function.child_by_field_name("lhs")?;
            let rhs = function.child_by_field_name("rhs")?;
            (crate::namespace_completion::unquote_package(node_text(lhs, content)) == "pacman"
                && crate::namespace_completion::unquote_package(node_text(rhs, content))
                    == "p_load")
                .then_some(PacmanCallee::Qualified)
        }
        "identifier" if node_text(function, content) == "p_load" => {
            let deferred = !super::binding::is_known_immediate_context(node);
            (!bindings
                .get()
                .named_local_binding_may_shadow_at("p_load", node, deferred))
            .then_some(PacmanCallee::Bare)
        }
        _ => None,
    }
}

/// Parse a statically-known pacman `p_load(...)` invocation.
///
/// An empty `calls` set means the callee is pacman's helper but its package
/// arguments are empty or not fully static. `evaluates_controls` distinguishes
/// a valid dynamic call (whose exact control values may execute) from malformed
/// argument matching such as duplicate exact controls, which errors before any
/// argument is forced.
struct ParsedPacmanPLoad {
    calls: Vec<LibraryCall>,
    evaluates_controls: bool,
}

fn try_parse_pacman_p_load_call(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Option<ParsedPacmanPLoad> {
    let callee = pacman_p_load_callee(node, content, bindings)?;
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Some(ParsedPacmanPLoad {
            calls: Vec::new(),
            evaluates_controls: false,
        });
    };
    if arguments.has_error() {
        return Some(ParsedPacmanPLoad {
            calls: Vec::new(),
            evaluates_controls: false,
        });
    }

    let mut positional = Vec::new();
    let mut char_value = None;
    let mut character_only = None;
    let mut saw_install = false;
    let mut saw_update = false;
    let mut invalid_control = false;
    let mut unknown_named_dot = false;
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            return Some(ParsedPacmanPLoad {
                calls: Vec::new(),
                evaluates_controls: false,
            });
        };
        let Some(name) = argument.child_by_field_name("name") else {
            positional.push(value);
            continue;
        };
        match node_text(name, content) {
            "char" => invalid_control |= char_value.replace(value).is_some(),
            "character.only" => {
                invalid_control |= character_only.replace(value).is_some();
            }
            "install" => invalid_control |= std::mem::replace(&mut saw_install, true),
            "update" => invalid_control |= std::mem::replace(&mut saw_update, true),
            // Formals after `...` match exactly in R. Partial or unrelated
            // names belong to dots, but treating their expressions as package
            // names would be too speculative unless exact `char` overrides
            // the entire dots list.
            _ => unknown_named_dot = true,
        }
    }
    if invalid_control {
        return Some(ParsedPacmanPLoad {
            calls: Vec::new(),
            evaluates_controls: false,
        });
    }
    if unknown_named_dot && char_value.is_none() {
        return Some(ParsedPacmanPLoad {
            calls: Vec::new(),
            evaluates_controls: true,
        });
    }

    if let Some(value) = character_only
        && node_text(value, content) != "FALSE"
    {
        // With TRUE pacman evaluates the first dots argument rather than using
        // its NSE spelling. That dynamic mode is outside this bounded detector.
        // An exact `char` still wins before character.only is consulted.
        if char_value.is_none() {
            return Some(ParsedPacmanPLoad {
                calls: Vec::new(),
                evaluates_controls: true,
            });
        }
    }

    let packages = if let Some(value) = char_value {
        // Upstream pacman gives exact `char` precedence over all dots.
        match value.kind() {
            "string" => extract_string_literal(value, content).map(|value| vec![value]),
            "identifier" => super::binding::plain_identifier_name(value, content)
                .and_then(|name| bindings.resolve_package_vector(name, node)),
            _ => {
                let packages = extract_c_strings_strict(value, content);
                packages.filter(|_| bindings.package_c_is_trusted_at(node))
            }
        }
    } else {
        positional
            .into_iter()
            .map(|value| match value.kind() {
                "identifier" => {
                    super::binding::plain_identifier_name(value, content).map(str::to_string)
                }
                "string" => extract_string_literal(value, content),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
    };
    let Some(packages) = packages else {
        return Some(ParsedPacmanPLoad {
            calls: Vec::new(),
            evaluates_controls: true,
        });
    };

    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let requires_attached = (callee == PacmanCallee::Bare).then(|| "pacman".to_string());
    Some(ParsedPacmanPLoad {
        calls: packages
            .into_iter()
            .map(|package| LibraryCall {
                package,
                line: end.row as u32,
                column: byte_offset_to_utf16_column(line_text, end.column),
                function_scope: None,
                attaches: true,
                requires_attached: requires_attached.clone(),
            })
            .collect(),
        evaluates_controls: true,
    })
}

// ============================================================================
// targets::tar_option_set Package Detection (issue #637)
// ============================================================================

/// A statically resolved worker package from a top-level {targets}
/// `tar_option_set(packages = ...)` call, pending the file-wide bare-callee
/// targets-in-play gate.
struct TarOptionSetCandidate {
    declaration: TargetsPackageDeclaration,
    bare: bool,
}

/// Whether `func_node` (a call's `function` child) names {targets}'
/// `tar_option_set`: the bare identifier `tar_option_set`, or the qualified
/// `targets::tar_option_set` / `targets:::tar_option_set`.
fn targets_callee_kind(func_node: Node, content: &str, fn_name: &str) -> Option<bool> {
    match func_node.kind() {
        "identifier" => {
            (crate::namespace_completion::unquote_package(node_text(func_node, content)) == fn_name)
                .then_some(true)
        }
        "namespace_operator" => {
            let lhs = func_node.child_by_field_name("lhs")?;
            let rhs = func_node.child_by_field_name("rhs")?;
            (lhs.kind() == "identifier"
                && rhs.kind() == "identifier"
                && crate::namespace_completion::unquote_package(node_text(lhs, content))
                    == "targets"
                && crate::namespace_completion::unquote_package(node_text(rhs, content)) == fn_name)
                .then_some(false)
        }
        _ => None,
    }
}

/// If `node` is a `tar_option_set(...)` call whose callee matches
/// [`targets_callee_kind`], parse it via
/// [`try_parse_tar_option_set_call`] and push one gate-pending candidate per
/// package, tagged with whether the callee was the bare spelling.
fn collect_tar_option_set_candidates(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
    tar_candidates: &mut Vec<TarOptionSetCandidate>,
) {
    let Some(func_node) = node.child_by_field_name("function") else {
        return;
    };
    let Some(bare) = targets_callee_kind(func_node, content, "tar_option_set") else {
        return;
    };
    if bare
        && bindings
            .get()
            .named_binding_may_shadow_at("tar_option_set", node, false)
    {
        return;
    }
    tar_candidates.extend(
        try_parse_tar_option_set_call(node, content, bindings)
            .into_iter()
            .map(|declaration| TarOptionSetCandidate { declaration, bare }),
    );
}

/// Move gate-approved `tar_option_set(packages = ...)` candidates into the
/// distinct pipeline-package channel.
///
/// Qualified calls are accepted unconditionally. A bare call requires an
/// attaching `library(targets)` / `require(targets)` somewhere in the same
/// evaluated file. This gate is deliberately position-independent, matching
/// the pipeline-level semantics; local callee shadowing is rejected earlier by
/// [`collect_tar_option_set_candidates`].
fn finalize_tar_option_set_candidates(output: &mut LibraryWalkOutput) {
    let mut ordered_calls: Vec<_> = output.library_calls.iter().collect();
    ordered_calls.sort_by_key(|detected| (detected.call.line, detected.call.column));
    let mut attached = std::collections::BTreeSet::new();
    for detected in ordered_calls {
        if detected.call.attaches
            && detected
                .call
                .requires_attached
                .as_ref()
                .is_none_or(|required| attached.contains(required.as_str()))
        {
            attached.insert(detected.call.package.as_str());
        }
    }
    let targets_attached = attached.contains("targets");
    for candidate in std::mem::take(&mut output.tar_candidates) {
        if !candidate.bare || targets_attached {
            output.targets_pipeline_packages.push(candidate.declaration);
        }
    }
    output.targets_pipeline_packages.sort_by(|left, right| {
        (&left.package, left.line, left.column).cmp(&(&right.package, right.line, right.column))
    });
    output
        .targets_pipeline_packages
        .dedup_by(|left, right| left.package == right.package);
}

/// Try to interpret `node` as a {targets} `tar_option_set(packages = ...)`
/// call and return one anchored [`TargetsPackageDeclaration`] per statically
/// determinable package.
///
/// Only the NAMED `packages =` argument is honored. This is an intentional
/// limitation: `tar_option_set`'s first formal is `tidy_eval`, not `packages`,
/// so positional matching (`tar_option_set(TRUE, c("dplyr"))`) would require
/// modeling the full formals list and is deliberately not attempted. Accepted
/// value shapes:
///
/// - a single string literal: `packages = "dplyr"`
/// - a strict `c()` of positional string literals:
///   `packages = c("dplyr", "tidyr")`
/// - an identifier resolving to a same-file, assigned-exactly-once static
///   `c()` of string literals bound before this call (the same
///   shared `StaticBindings::resolve_package_vector` machinery apply-family detection uses)
///
/// Variable candidates must be unconditional top-level bindings. Nested,
/// conditional, removed, reassigned, or destination-qualified bindings still
/// invalidate the name but cannot supply a package vector.
///
/// Anything else (dynamic calls, `character(0)`, empty `c()`) yields nothing.
///
/// Position anchoring: `tar_option_set()` calls routinely span 10–30 lines,
/// and the missing-package diagnostic builds its range from a `TargetsPackageDeclaration`'s
/// line/column while `# raven: ignore` suppression is line-keyed — so for the
/// string-literal and `c()`-of-literals shapes, each package's `TargetsPackageDeclaration`
/// carries the end position of ITS OWN string-literal node (not the call's
/// closing paren). The variable-resolved shape has no literal at the call
/// site, so it falls back to the call's end position, mirroring the
/// apply-family pattern.
///
/// Union leniency: multiple `tar_option_set` calls in one file union their
/// packages. targets' real runtime semantics are last-call-wins for
/// `packages =`, but raven deliberately favors false negatives (a package
/// wrongly considered available) over false positives (flagging a defined
/// verb as undefined) — do not "fix" this to last-call-wins.
///
/// The caller is responsible for the bare-vs-qualified targets-in-play gate
/// (see [`finalize_tar_option_set_candidates`]); this function parses any
/// callee matching [`targets_callee_kind`].
fn try_parse_tar_option_set_call(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Vec<TargetsPackageDeclaration> {
    let Some(func_node) = node.child_by_field_name("function") else {
        return Vec::new();
    };
    if targets_callee_kind(func_node, content, "tar_option_set").is_none() {
        return Vec::new();
    }
    let Some(args_node) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    if args_node.has_error() {
        return Vec::new();
    }

    // Find the named `packages =` argument (exact name only). A duplicate
    // named `packages` argument would make the call itself error in R, so
    // treat that as no match.
    let mut packages_values: Vec<Node> = Vec::new();
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument"
            && let Some(name_node) = child.child_by_field_name("name")
            && node_text(name_node, content) == "packages"
            && let Some(value_node) = child.child_by_field_name("value")
        {
            packages_values.push(value_node);
        }
    }
    let [value_node] = packages_values.as_slice() else {
        return Vec::new();
    };

    // Build an anchored declaration at `anchor`'s end position (line + UTF-16
    // column), matching how the other detectors in this file anchor.
    let declaration_at = |package: String, anchor: Node| -> TargetsPackageDeclaration {
        let end = anchor.end_position();
        let line_text = content.lines().nth(end.row).unwrap_or("");
        TargetsPackageDeclaration {
            package,
            line: end.row as u32,
            column: byte_offset_to_utf16_column(line_text, end.column),
        }
    };

    match value_node.kind() {
        // `packages = "dplyr"` — anchored at the literal itself.
        "string" => match extract_string_literal(*value_node, content) {
            Some(package) => vec![declaration_at(package, *value_node)],
            None => Vec::new(),
        },
        // `pkgs <- c("a", "b"); tar_option_set(packages = pkgs)` — no literal
        // at the call site, so anchor at the call's end position.
        "identifier" => {
            let Some(text) = super::binding::plain_identifier_name(*value_node, content) else {
                return Vec::new();
            };
            match bindings.resolve_package_vector(text, node) {
                Some(packages) => packages
                    .into_iter()
                    .map(|package| declaration_at(package, node))
                    .collect(),
                None => Vec::new(),
            }
        }
        // `packages = c("dplyr", "tidyr")` — one call per package, each
        // anchored at its own string-literal node.
        _ => {
            let Some(pairs) = super::binding::extract_bare_c_plain_strings(*value_node, content)
            else {
                return Vec::new();
            };
            if !bindings.package_c_is_trusted_at(node) {
                return Vec::new();
            }
            pairs
                .into_iter()
                .map(|(package, literal)| declaration_at(package, literal))
                .collect()
        }
    }
}

/// Detect the file/pipeline-level worker package set declared by statically
/// recognized top-level `tar_option_set(packages = ...)` calls.
///
/// Bare calls require `{targets}` to be attached somewhere in the same file and
/// must not be shadowed by a local binding. Qualified `targets::` / `targets:::`
/// calls are unconditional. Multiple calls union their statically resolved
/// packages; unsupported dynamic forms contribute nothing.
pub fn detect_targets_pipeline_packages(
    tree: &Tree,
    content: &str,
) -> Vec<TargetsPackageDeclaration> {
    let root = tree.root_node();
    let mut bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_library_walk_output(tree, content, &mut bindings).targets_pipeline_packages
}

// ============================================================================
// targets::tar_source Detection (issue #648)
// ============================================================================

struct TarSourceCandidate {
    request: TarSourceRequest,
    bare: bool,
}

/// Detect statically resolvable, top-level `{targets}` `tar_source()` calls.
///
/// Qualified calls need no attachment gate. A bare call is accepted only when
/// the same evaluated top-level code attaches `{targets}`. Proven capture
/// wrappers are traversed only through their evaluated controls/splices, using
/// the same capture-aware binding machinery as source and package detection.
pub fn detect_tar_source_requests(tree: &Tree, content: &str) -> Vec<TarSourceRequest> {
    let root = tree.root_node();
    let mut bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_tar_source_requests_with_bindings(root, content, &mut bindings)
}

fn detect_tar_source_requests_with_bindings(
    root: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Vec<TarSourceRequest> {
    let mut candidates = Vec::new();
    let mut targets_attached = false;
    visit_node_for_tar_source(
        root,
        content,
        bindings,
        RuntimeFunctionScope::Lexical,
        &mut targets_attached,
        &mut candidates,
    );
    candidates
        .into_iter()
        .filter(|candidate| !candidate.bare || targets_attached)
        .map(|candidate| candidate.request)
        .collect()
}

/// Detect library calls and tar requests while sharing one lazy binding table.
pub(crate) fn detect_library_and_tar_source_requests(
    tree: &Tree,
    content: &str,
) -> (
    Vec<LibraryCall>,
    Vec<TargetsPackageDeclaration>,
    Vec<TarSourceRequest>,
    Vec<ListFilesSourceRequest>,
    super::targets::TargetsMetadata,
) {
    let root = tree.root_node();
    let mut bindings = super::static_path::LazyStaticBindings::new(root, content);
    let package_output = detect_library_walk_output(tree, content, &mut bindings);
    let attaching_calls =
        top_level_attaching_library_calls_with_bindings(tree, content, &mut bindings);
    let targets_metadata =
        super::targets::detect_targets_metadata(root, content, &mut bindings, &attaching_calls);
    let library_calls = package_output
        .library_calls
        .into_iter()
        .map(|detected| detected.call)
        .collect();
    let targets_pipeline_packages = package_output.targets_pipeline_packages;
    let requests = detect_tar_source_requests_with_bindings(root, content, &mut bindings);
    let list_files_requests =
        detect_list_files_source_requests_with_bindings(root, content, &mut bindings);
    (
        library_calls,
        targets_pipeline_packages,
        requests,
        list_files_requests,
        targets_metadata,
    )
}

/// Detect the deliberately bounded directory-source idiom:
///
/// ```r
/// files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
/// for (file in files) source(file)
/// ```
///
/// The assignment and loop must be adjacent executable top-level statements.
/// This keeps evaluation order, helper shadowing, and the sequence binding
/// syntax-local; filesystem enumeration remains a detached later phase.
pub fn detect_list_files_source_requests(
    tree: &Tree,
    content: &str,
) -> Vec<ListFilesSourceRequest> {
    let root = tree.root_node();
    let mut bindings = super::static_path::LazyStaticBindings::new(root, content);
    detect_list_files_source_requests_with_bindings(root, content, &mut bindings)
}

fn detect_list_files_source_requests_with_bindings(
    root: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Vec<ListFilesSourceRequest> {
    if !content.contains("list.files") || !content.contains("source") {
        return Vec::new();
    }
    let mut cursor = root.walk();
    let statements: Vec<_> = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect();
    statements
        .windows(2)
        .filter_map(|pair| parse_list_files_source_pair(pair[0], pair[1], content, bindings))
        .collect()
}

fn parse_list_files_source_pair(
    assignment: Node,
    loop_node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Option<ListFilesSourceRequest> {
    if assignment.kind() != "binary_operator" || loop_node.kind() != "for_statement" {
        return None;
    }
    let operator = assignment.child_by_field_name("operator")?;
    if !matches!(node_text(operator, content), "<-" | "=") {
        return None;
    }
    let assigned_name =
        super::binding::plain_identifier_name(assignment.child_by_field_name("lhs")?, content)?;
    let sequence_name =
        super::binding::plain_identifier_name(loop_node.child_by_field_name("sequence")?, content)?;
    if assigned_name != sequence_name {
        return None;
    }
    let directory =
        parse_bounded_list_files_call(assignment.child_by_field_name("rhs")?, content, bindings)?;

    let iterator =
        super::binding::plain_identifier_name(loop_node.child_by_field_name("variable")?, content)?;
    let body = loop_node.child_by_field_name("body")?;
    let source_call = if body.kind() == "call" {
        body
    } else if body.kind() == "braced_expression" {
        let mut cursor = body.walk();
        let mut statements = body
            .named_children(&mut cursor)
            .filter(|node| node.kind() != "comment");
        let call = statements.next()?;
        if statements.next().is_some() {
            return None;
        }
        call
    } else {
        return None;
    };
    if !is_bounded_iterator_source_call(source_call, iterator, loop_node, content, bindings) {
        return None;
    }
    let start = source_call.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    Some(ListFilesSourceRequest {
        directory,
        line: start.row as u32,
        column: byte_offset_to_utf16_column(line_text, start.column),
    })
}

fn parse_bounded_list_files_call(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Option<String> {
    if node.kind() != "call" || node.has_error() {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    let bare = match function.kind() {
        "identifier" if node_text(function, content) == "list.files" => true,
        "namespace_operator" => {
            let lhs = function.child_by_field_name("lhs")?;
            let rhs = function.child_by_field_name("rhs")?;
            if node_text(lhs, content) != "base" || node_text(rhs, content) != "list.files" {
                return None;
            }
            false
        }
        _ => return None,
    };
    if bare
        && bindings
            .get()
            .named_local_binding_may_shadow_without_helper_uncertainty("list.files", node, false)
    {
        return None;
    }

    let arguments = node.child_by_field_name("arguments")?;
    let mut directory = None;
    let mut pattern = None;
    let mut full_names = false;
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let value = argument.child_by_field_name("value")?;
        match argument
            .child_by_field_name("name")
            .map(|name| node_text(name, content))
        {
            None if directory.is_none() => {
                directory = Some(extract_string_literal(value, content)?)
            }
            Some("path") if directory.is_none() => {
                directory = Some(extract_string_literal(value, content)?);
            }
            Some("pattern") if pattern.is_none() => {
                pattern = Some(extract_string_literal(value, content)?);
            }
            Some("full.names") if !full_names && node_text(value, content) == "TRUE" => {
                full_names = true;
            }
            _ => return None,
        }
    }
    if pattern.as_deref() != Some(r"\\.R$") || !full_names {
        return None;
    }
    directory
}

fn is_bounded_iterator_source_call(
    node: Node,
    iterator: &str,
    loop_node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> bool {
    if node.kind() != "call" || node.has_error() {
        return false;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return false;
    };
    let bare = match function.kind() {
        "identifier" if node_text(function, content) == "source" => true,
        "namespace_operator" => {
            let Some(lhs) = function.child_by_field_name("lhs") else {
                return false;
            };
            let Some(rhs) = function.child_by_field_name("rhs") else {
                return false;
            };
            if node_text(lhs, content) != "base" || node_text(rhs, content) != "source" {
                return false;
            }
            false
        }
        _ => return false,
    };
    if bare
        && (iterator == "source"
            || bindings
                .get()
                .named_local_binding_may_shadow_without_helper_uncertainty(
                    "source", loop_node, false,
                ))
    {
        return false;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut values = Vec::new();
    let mut cursor = arguments.walk();
    for argument in arguments.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        if argument.child_by_field_name("name").is_some() {
            return false;
        }
        let Some(value) = argument.child_by_field_name("value") else {
            return false;
        };
        values.push(value);
    }
    values.len() == 1 && super::binding::plain_identifier_name(values[0], content) == Some(iterator)
}

fn visit_node_for_tar_source(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
    runtime_function_scope: RuntimeFunctionScope,
    targets_attached: &mut bool,
    candidates: &mut Vec<TarSourceCandidate>,
) {
    if node.kind() == "identifier" {
        return;
    }

    if node.kind() == "call" {
        if let Some(capture) = bindings.capturing_call_kind_at(node) {
            let captured_runtime_scope = runtime_function_scope.for_evaluated_capture_part(node);
            super::binding::visit_evaluated_capture_parts(
                node,
                content,
                capture,
                &mut |evaluated, _frame, _role| {
                    visit_node_for_tar_source(
                        evaluated,
                        content,
                        bindings,
                        captured_runtime_scope,
                        targets_attached,
                        candidates,
                    );
                },
            );
            return;
        }

        if !runtime_function_scope.is_function_scoped_at(node) {
            if call_attaches_targets(node, content, bindings) {
                *targets_attached = true;
            }
            if let Some(func_node) = node.child_by_field_name("function")
                && let Some(bare) = targets_callee_kind(func_node, content, "tar_source")
                && (!bare
                    || !bindings
                        .get()
                        .named_binding_may_shadow_at("tar_source", node, false))
                && let Some(request) = try_parse_tar_source_call(node, content, bindings)
            {
                candidates.push(TarSourceCandidate { request, bare });
            }
        }
    }

    let child_runtime_scope = if node.kind() == "function_definition" {
        runtime_function_scope.enter_function()
    } else {
        runtime_function_scope
    };
    for child in node.children(&mut node.walk()) {
        visit_node_for_tar_source(
            child,
            content,
            bindings,
            child_runtime_scope,
            targets_attached,
            candidates,
        );
    }
}

fn call_attaches_targets(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> bool {
    if try_parse_apply_library_call(node, content, bindings)
        .iter()
        .any(|call| call.attaches && call.package == "targets")
    {
        return true;
    }
    let Some(function) = node.child_by_field_name("function") else {
        return false;
    };
    let loader = match function.kind() {
        "identifier" => crate::namespace_completion::unquote_package(node_text(function, content)),
        "namespace_operator" => {
            let Some(lhs) = function.child_by_field_name("lhs") else {
                return false;
            };
            let Some(rhs) = function.child_by_field_name("rhs") else {
                return false;
            };
            if crate::namespace_completion::unquote_package(node_text(lhs, content)) != "base" {
                return false;
            }
            crate::namespace_completion::unquote_package(node_text(rhs, content))
        }
        _ => return false,
    };
    if !matches!(loader, "library" | "require") {
        return false;
    }
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return false;
    };
    if arguments.has_error() || has_character_only_true(&arguments, content) {
        return false;
    }
    extract_package_name(&arguments, content)
        .is_some_and(|package| crate::namespace_completion::unquote_package(&package) == "targets")
}

/// Parse `tar_source(files, envir, change_directory)` using R argument matching.
fn try_parse_tar_source_call(
    node: Node,
    content: &str,
    bindings: &mut super::static_path::LazyStaticBindings,
) -> Option<TarSourceRequest> {
    let args = node.child_by_field_name("arguments")?;
    if args.has_error() {
        return None;
    }
    const FORMALS: [&str; 3] = ["files", "envir", "change_directory"];
    let mut values: [Option<Node>; 3] = [None, None, None];
    let mut bound = [false; 3];
    let mut partial_named = Vec::new();
    let mut positional = Vec::new();
    let mut cursor = args.walk();
    for argument in args.children(&mut cursor) {
        if argument.kind() != "argument" {
            continue;
        }
        let value = argument.child_by_field_name("value");
        if let Some(name_node) = argument.child_by_field_name("name") {
            let name = crate::namespace_completion::unquote_package(node_text(name_node, content));
            if let Some(index) = FORMALS.iter().position(|formal| *formal == name) {
                if bound[index] {
                    return None;
                }
                bound[index] = true;
                values[index] = value;
            } else {
                partial_named.push((name, value));
            }
        } else {
            positional.push(value);
        }
    }
    for (name, value) in partial_named {
        let matches: Vec<_> = FORMALS
            .iter()
            .enumerate()
            .filter_map(|(index, formal)| formal.starts_with(name).then_some(index))
            .collect();
        let [index] = matches.as_slice() else {
            return None;
        };
        if bound[*index] {
            return None;
        }
        bound[*index] = true;
        values[*index] = value;
    }
    for value in positional {
        let index = bound.iter().position(|is_bound| !is_bound)?;
        bound[index] = true;
        values[index] = value;
    }

    if values[1].is_some() {
        return None;
    }
    let change_directory = match values[2] {
        None => false,
        Some(value) => match node_text(value, content) {
            "TRUE" => true,
            "FALSE" => false,
            _ => return None,
        },
    };
    let files = match values[0] {
        None => vec!["R".to_string()],
        Some(value) => match value.kind() {
            "string" => vec![extract_string_literal(value, content)?],
            "identifier" => {
                let name = super::binding::plain_identifier_name(value, content)?;
                bindings.resolve_package_vector(name, node)?
            }
            _ => {
                let pairs = super::binding::extract_bare_c_plain_strings(value, content)?;
                if !bindings.package_c_is_trusted_at(node) {
                    return None;
                }
                pairs.into_iter().map(|(string, _)| string).collect()
            }
        },
    };
    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    Some(TarSourceRequest {
        files,
        line: start.row as u32,
        column: byte_offset_to_utf16_column(line_text, start.column),
        change_directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn tar_requests(code: &str) -> Vec<TarSourceRequest> {
        detect_tar_source_requests(&parse_r(code), code)
    }

    fn list_files_requests(code: &str) -> Vec<ListFilesSourceRequest> {
        detect_list_files_source_requests(&parse_r(code), code)
    }

    #[test]
    fn detects_bounded_top_level_list_files_source_loops() {
        for (code, expected_line) in [
            (
                r#"files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
                   for (file in files) source(file)"#,
                1,
            ),
            (
                r#"files = base::list.files(path = "functions", full.names = TRUE, pattern = "\\.R$")
                   # comments do not break adjacency
                   for (file in files) { base:::source(file) }"#,
                2,
            ),
        ] {
            let requests = list_files_requests(code);
            assert_eq!(requests.len(), 1, "{code}: {requests:?}");
            assert_eq!(requests[0].directory, "functions");
            assert_eq!(requests[0].line, expected_line);
        }
    }

    #[test]
    fn list_files_source_loops_fail_closed_outside_the_bounded_shape() {
        for code in [
            r#"files <- list.files("functions", pattern = "\\.R$")
               for (file in files) source(file)"#,
            r#"files <- list.files("functions", pattern = "\\.R$", full.names = FALSE)
               for (file in files) source(file)"#,
            r#"files <- list.files("functions", pattern = "\\.[Rr]$", full.names = TRUE)
               for (file in files) source(file)"#,
            r#"files <- list.files(directory, pattern = "\\.R$", full.names = TRUE)
               for (file in files) source(file)"#,
            r#"files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               message("intervening")
               for (file in files) source(file)"#,
            r#"files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               for (file in files) { source(file); message(file) }"#,
            r#"files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               for (file in files) source(file, chdir = FALSE)"#,
            r#"source <- function(...) NULL
               files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               for (file in files) source(file)"#,
            r#"list.files <- function(...) "wrong.R"
               files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               for (file in files) source(file)"#,
            r#"files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               for (file in other) source(file)"#,
            r#"files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
               for (source in files) source(source)"#,
            r#"f <- function() {
                 files <- list.files("functions", pattern = "\\.R$", full.names = TRUE)
                 for (file in files) source(file)
               }"#,
        ] {
            assert!(
                list_files_requests(code).is_empty(),
                "{code}: {:?}",
                list_files_requests(code)
            );
        }
    }

    #[test]
    fn matching_file_exists_guard_marks_only_the_source_diagnostic() {
        for code in [
            r#"if (file.exists("scripts/config.R")) source("scripts/config.R")"#,
            r#"if (base::file.exists("scripts/config.R")) { source("scripts/config.R") }"#,
            r#"if (base:::file.exists("scripts/config.R")) {
                # A comment does not make this a multi-expression consequence.
                source("scripts/config.R")
            }"#,
            r#"if (file.exists("scripts/config.R")) {
                source("scripts/config.R")
            } else {
                message("Using built-in defaults")
            }"#,
            r#"source(dynamic_path)
               if (file.exists("scripts/config.R")) source("scripts/config.R")"#,
            r#"if (file.exists("scripts/config.R")) source("scripts/config.R")
               file.exists <- function(...) TRUE"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert!(sources[0].guarded_by_file_exists, "{code}: {sources:?}");
            assert_eq!(sources[0].path, "scripts/config.R");
        }
    }

    #[test]
    fn file_exists_guard_fails_closed_for_broader_control_flow() {
        for code in [
            r#"if (file.exists("other.R")) source("config.R")"#,
            r#"if (!file.exists("config.R")) source("config.R")"#,
            r#"if (file.exists(path)) source("config.R")"#,
            r#"if (file.exists("config.R", "other.R")) source("config.R")"#,
            r#"if (file.exists(file = "config.R")) source("config.R")"#,
            r#"if (file.exists("config.R")) { x <- 1; source("config.R") }"#,
            r#"if (file.exists("config.R")) NULL else source("config.R")"#,
            r#"file.exists <- function(...) TRUE
               if (file.exists("config.R")) source("config.R")"#,
            r#"name <- "file.exists"
               assign(name, function(...) TRUE)
               if (file.exists("config.R")) source("config.R")"#,
            r#"defer <- function(x) function() x
               g <- defer(if (file.exists("config.R")) source("config.R"))
               file.exists <- function(...) TRUE
               g()"#,
            r#"if (file.exists("config.R")) print("not a source")
               source("config.R")"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert!(!sources[0].guarded_by_file_exists, "{code}: {sources:?}");
        }
    }

    #[test]
    fn tar_source_bare_gate_is_top_level_and_position_independent() {
        let requests = tar_requests("tar_source()\nlibrary(targets)");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].files, ["R"]);

        assert!(tar_requests("tar_source <- function(...) {}\ntar_source()").is_empty());
        assert!(tar_requests("f <- function() library(targets)\ntar_source()").is_empty());
        assert!(tar_requests("quote(library(targets))\ntar_source()").is_empty());
    }

    #[test]
    fn tar_source_qualified_calls_and_capture_boundaries() {
        let requests = tar_requests(
            "targets::tar_source(\"one.R\")\ntargets:::tar_source(c(\"two.R\", \"dir\"))",
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].files, ["one.R"]);
        assert_eq!(requests[1].files, ["two.R", "dir"]);

        assert!(tar_requests("f <- function() targets::tar_source(\"x.R\")").is_empty());
        assert!(tar_requests("quote(targets::tar_source(\"x.R\"))").is_empty());
        assert!(tar_requests("rlang::expr(targets::tar_source(\"x.R\"))").is_empty());
        assert_eq!(
            tar_requests("rlang::expr(!!targets::tar_source(\"x.R\"))").len(),
            1
        );
    }

    #[test]
    fn tar_source_accepts_backtick_callee_and_formal_spellings() {
        let requests =
            tar_requests("targets::`tar_source`(`files` = \"one.R\", `change_directory` = TRUE)");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].files, ["one.R"]);
        assert!(requests[0].change_directory);
    }

    #[test]
    fn tar_source_static_vectors_reuse_capture_aware_bindings() {
        let requests = tar_requests("paths <- c(\"R\", \"setup.R\")\ntargets::tar_source(paths)");
        assert_eq!(requests[0].files, ["R", "setup.R"]);

        for code in [
            "targets::tar_source(paths)\npaths <- c(\"R\")",
            "paths <- c(\"R\")\npaths <- c(\"more\")\ntargets::tar_source(paths)",
            "if (ok) paths <- c(\"R\")\ntargets::tar_source(paths)",
            "paths <- c(\"R\")\npaths[1] <- \"other\"\ntargets::tar_source(paths)",
            "paths <- c(\"R\")\nfor (paths in \"other\") {}\ntargets::tar_source(paths)",
            "paths <- c(\"R\")\nrm(paths)\ntargets::tar_source(paths)",
            "paths <- c(\"R\")\nassign(\"paths\", \"other.R\")\ntargets::tar_source(paths)",
        ] {
            assert!(
                tar_requests(code).is_empty(),
                "unexpected request for: {code}"
            );
        }
    }

    #[test]
    fn tar_source_matches_formals_and_static_flags() {
        let request = tar_requests("targets::tar_source(ch = TRUE, fil = c(\"R\", \"setup.R\"))");
        assert_eq!(request.len(), 1);
        assert!(request[0].change_directory);
        assert_eq!(request[0].files, ["R", "setup.R"]);

        for code in [
            "targets::tar_source(files =)",
            "targets::tar_source(envir =)",
            "targets::tar_source(change_directory =)",
            "targets::tar_source(, ,)",
        ] {
            let requests = tar_requests(code);
            assert_eq!(requests.len(), 1, "expected defaults for: {code}");
            assert_eq!(requests[0].files, ["R"]);
            assert!(!requests[0].change_directory);
        }

        for code in [
            "targets::tar_source(\"x.R\", globalenv())",
            "targets::tar_source(files = \"x.R\", files = \"y.R\")",
            "targets::tar_source(files = \"x.R\", fil = \"y.R\")",
            "targets::tar_source(\"x.R\", change_directory = flag)",
            "targets::tar_source(\"x.R\", change_directory = T)",
            "targets::tar_source(unknown = \"x.R\")",
        ] {
            assert!(
                tar_requests(code).is_empty(),
                "unexpected request for: {code}"
            );
        }
    }

    #[test]
    fn detects_folded_file_path_source_call() {
        let code = r#"source(file.path("scripts", "helpers.R"), local = TRUE)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "got: {sources:?}");
        assert_eq!(sources[0].path, "scripts/helpers.R");
        assert!(sources[0].locality != SourceLocality::Global);
        assert!(!sources[0].is_directive);
        assert!(sources[0].system_file.is_none());
    }

    #[test]
    fn detects_issue_638_repro_computed_source() {
        // The exact idiom from issue #638: a testthat helper computing the
        // repo root and sourcing project code through it.
        let code = r#"
repo_root <- normalizePath(file.path("..", ".."))
source(file.path(repo_root, "scripts/helpers.R"))
"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "got: {sources:?}");
        assert_eq!(sources[0].path, "../../scripts/helpers.R");
        assert_eq!(sources[0].line, 2);
        assert!(!sources[0].is_function_scoped);
    }

    #[test]
    fn computed_source_respects_load_invalidation_destination_and_runtime_scope() {
        let cases = [
            (
                "p <- \"good.R\"\nbase::load(\"state.RData\")\nsource(p)\n",
                false,
            ),
            (
                "base::load(\"state.RData\")\np <- \"good.R\"\nsource(p)\n",
                true,
            ),
            (
                "p <- \"good.R\"\nbase::load(\"state.RData\", envir = base::new.env())\nsource(p)\n",
                true,
            ),
            (
                "p <- \"good.R\"\nf <- function() base::load(\"state.RData\")\nsource(p)\n",
                true,
            ),
            (
                "p <- \"good.R\"\nf <- function() { base::load(\"state.RData\"); source(p) }\n",
                false,
            ),
            (
                "p <- \"good.R\"\nf <- function() base::load(\"state.RData\", envir = .GlobalEnv)\nsource(p)\n",
                false,
            ),
        ];
        for (code, detected) in cases {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(!sources.is_empty(), detected, "{code}\n{sources:?}");
            if detected {
                assert_eq!(
                    sources.last().map(|source| source.path.as_str()),
                    Some("good.R")
                );
            }
        }
    }

    #[test]
    fn computed_source_with_unfoldable_path_still_ignored() {
        let code = r#"source(file.path(Sys.getenv("ROOT"), "x.R"))"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources.is_empty(), "got: {sources:?}");
    }

    #[test]
    fn folded_sys_source_detected() {
        let code = r#"sys.source(file.path("R", "utils.R"), envir = globalenv())"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "got: {sources:?}");
        assert_eq!(sources[0].path, "R/utils.R");
        assert!(sources[0].is_sys_source);
        assert!(sources[0].locality == SourceLocality::Global);
    }

    #[test]
    fn ns_ref_basic_exported() {
        let code = "dplyr::mutate(x)\n";
        let tree = parse_r(code);
        let refs = detect_namespace_references(&tree, code);
        assert_eq!(refs.len(), 1, "got: {refs:?}");
        let r = &refs[0];
        assert_eq!(r.package, "dplyr");
        assert!(!r.internal);
        let m = r.member.as_ref().expect("member present");
        assert_eq!(m.name, "mutate");
        // package range covers "dplyr" at columns 0..5
        assert_eq!(
            (
                r.package_range.start_line,
                r.package_range.start_column,
                r.package_range.end_column
            ),
            (0, 0, 5)
        );
    }

    #[test]
    fn ns_ref_internal_triple_colon() {
        let refs =
            detect_namespace_references(&parse_r("dplyr:::peek_mask()\n"), "dplyr:::peek_mask()\n");
        assert_eq!(refs.len(), 1);
        assert!(refs[0].internal);
        assert_eq!(refs[0].member.as_ref().unwrap().name, "peek_mask");
    }

    #[test]
    fn ns_ref_quoted_lhs_and_rhs() {
        for code in [
            r#""dplyr"::mutate"#,
            "`dplyr`::mutate",
            r#"dplyr::"mutate""#,
        ] {
            let src = format!("{code}\n");
            let refs = detect_namespace_references(&parse_r(&src), &src);
            assert_eq!(refs.len(), 1, "code: {code} got: {refs:?}");
            assert_eq!(refs[0].package, "dplyr", "code: {code}");
            assert_eq!(
                refs[0].member.as_ref().unwrap().name,
                "mutate",
                "code: {code}"
            );
        }
    }

    #[test]
    fn ns_ref_nonsyntactic_member() {
        let src = "pkg::`non syntactic`\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].member.as_ref().unwrap().name, "non syntactic");
    }

    #[test]
    fn ns_ref_incomplete_records_none_member() {
        // Incomplete `pkg::` parsed as a namespace_operator with no rhs is kept
        // for warming with member: None.
        let src = "library(dplyr)\ndplyr::\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        let incomplete: Vec<_> = refs.iter().filter(|r| r.member.is_none()).collect();
        assert_eq!(incomplete.len(), 1, "got: {refs:?}");
        assert_eq!(incomplete[0].package, "dplyr");
    }

    #[test]
    fn ns_ref_ignores_invalid_lhs_and_comments() {
        // a$b::x has a non-identifier/non-string LHS -> ignored.
        // A `::` inside a comment or string is not a namespace_operator node.
        let src = "a$b::x\n# dplyr::mutate\ny <- \"tidyr::pivot\"\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        assert!(refs.is_empty(), "got: {refs:?}");
    }

    #[test]
    fn ns_ref_utf16_columns() {
        // 🎉 is 2 UTF-16 units; pkg starts at column 2.
        let src = "🎉; dplyr::mutate\n";
        let refs = detect_namespace_references(&parse_r(src), src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].package_range.start_column, 4); // 2 (emoji) + 2 ("; ")
    }

    // ------------------------------------------------------------------
    // extract_attached_packages (issue #432): top-level attaching
    // library()/require() only; loadNamespace and function-body calls excluded.
    // ------------------------------------------------------------------

    #[test]
    fn extract_attached_packages_direct_library_and_require() {
        let pkgs = extract_attached_packages("library(tidyr)\nrequire(dplyr)\n");
        assert!(pkgs.contains("tidyr"), "got: {pkgs:?}");
        assert!(pkgs.contains("dplyr"), "got: {pkgs:?}");
        assert_eq!(pkgs.len(), 2, "got: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_string_and_named_arg() {
        let pkgs = extract_attached_packages("library(\"tidyr\")\nlibrary(package = dplyr)\n");
        assert!(pkgs.contains("tidyr"), "got: {pkgs:?}");
        assert!(pkgs.contains("dplyr"), "got: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_excludes_load_namespace() {
        // loadNamespace loads the namespace for qualified access but does not
        // attach exports to the search path.
        let pkgs = extract_attached_packages("loadNamespace(\"tidyr\")\n");
        assert!(pkgs.is_empty(), "loadNamespace must not attach: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_excludes_function_body_calls() {
        // A library() inside a function body does not attach until the function
        // is called, so testthat sourcing the preamble does not attach it.
        let pkgs = extract_attached_packages("f <- function() library(stringr)\n");
        assert!(
            pkgs.is_empty(),
            "function-body library() must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_lambda_body_calls() {
        // R 4.1+ lambda bodies are `function_definition` nodes too.
        let pkgs = extract_attached_packages("g <- \\() library(stringr)\n");
        assert!(
            pkgs.is_empty(),
            "lambda-body library() must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_includes_top_level_local_block() {
        // `local({ ... })` evaluates immediately when the file is sourced, so a
        // `library()` inside it DOES attach (it is not a function body).
        let pkgs = extract_attached_packages("local({\n  library(tidyr)\n})\n");
        assert!(
            pkgs.contains("tidyr"),
            "top-level local() library() must attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_includes_top_level_if_block() {
        let pkgs = extract_attached_packages("if (TRUE) {\n  library(tidyr)\n}\n");
        assert!(
            pkgs.contains("tidyr"),
            "top-level if-block library() must attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_includes_suppress_messages_wrapper() {
        // A wrapper call at top level (e.g. suppressMessages(library(x))) still
        // attaches — the inner library() is reached on recursion, no function
        // body intervenes.
        let pkgs = extract_attached_packages("suppressMessages(library(tidyr))\n");
        assert!(
            pkgs.contains("tidyr"),
            "suppressMessages(library()) must attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_quote_wrapper() {
        // `quote()` captures the call as an unevaluated expression; sourcing the
        // preamble never attaches the package.
        let pkgs = extract_attached_packages("quote(library(tidyr))\n");
        assert!(
            pkgs.is_empty(),
            "quote(library()) must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_rlang_expr_wrapper() {
        // Namespace-qualified rlang quoting function: also non-evaluating.
        let pkgs = extract_attached_packages("e <- rlang::expr(library(tidyr))\n");
        assert!(
            pkgs.is_empty(),
            "rlang::expr(library()) must not attach: {pkgs:?}"
        );
    }

    #[test]
    fn extract_attached_packages_excludes_bquote_and_expression() {
        assert!(extract_attached_packages("bquote(library(tidyr))\n").is_empty());
        assert!(extract_attached_packages("expression(library(tidyr))\n").is_empty());
        assert!(extract_attached_packages("substitute(library(tidyr))\n").is_empty());
    }

    #[test]
    fn extract_attached_packages_empty_on_unparseable() {
        // Robustness: never panics, returns empty on garbage.
        let result = extract_attached_packages("library(\n");
        assert!(
            result.is_empty(),
            "malformed input must yield no attached packages: {result:?}"
        );
    }

    #[test]
    fn test_source_double_quotes() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
        assert!(!sources[0].is_sys_source);
        assert!(sources[0].locality == SourceLocality::Global);
        assert!(!sources[0].chdir);
    }

    #[test]
    fn test_source_single_quotes() {
        let code = "source('utils.R')";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
    }

    #[test]
    fn source_detection_excludes_proven_captured_code() {
        for code in [
            r#"quote(source("quote.R"))"#,
            r#"base::quote(source("base-quote.R"))"#,
            r#"expression(source("expression.R"))"#,
            r#"rlang::expr(source("expr.R"))"#,
            r#"rlang::quo(source("quo.R"))"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert!(sources.is_empty(), "{code}: {sources:?}");
        }
    }

    #[test]
    fn source_detection_traverses_evaluated_capture_parts() {
        let cases = [
            (r#"bquote(.(source("splice.R")))"#, "splice.R"),
            (
                r#"bquote(list(..(source("splice-list.R"))), splice = TRUE)"#,
                "splice-list.R",
            ),
            (
                r#"bquote(x, where = { source("where.R"); parent.frame() })"#,
                "where.R",
            ),
            (
                r#"substitute(x, env = { source("env.R"); globalenv() })"#,
                "env.R",
            ),
            (r#"rlang::expr(!!source("unquote.R"))"#, "unquote.R"),
        ];
        for (code, expected) in cases {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(sources[0].path, expected, "{code}");
        }

        for code in [
            r#"bquote(source("captured.R"))"#,
            r#"substitute(source("captured.R"), env = globalenv())"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert!(sources.is_empty(), "{code}: {sources:?}");
        }
    }

    #[test]
    fn source_detection_preserves_unshadowed_quote_inside_immediate_bquote_operand() {
        let code = r#"
            p <- "good.R"
            bquote(.(quote(p <- "bad.R")))
            source(p)
        "#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "good.R");

        let code = r#"
            quote <- function(x) force(x)
            p <- "good.R"
            bquote(.(quote(p <- "bad.R")))
            source(p)
        "#;
        assert!(detect_source_calls(&parse_r(code), code).is_empty());
    }

    #[test]
    fn root_bquote_splice_keeps_where_source_before_operand_error() {
        let code = r#"bquote(..(source("operand.R")), where = { source("where.R"); parent.frame() }, splice = TRUE)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "where.R");
    }

    #[test]
    fn bquote_where_removal_prevents_computed_source_edge() {
        let code = r#"
        p <- "child.R"
        bquote(.(source(p)), where = { rm(p); parent.frame() })
        "#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources.is_empty(), "{sources:?}");
    }

    #[test]
    fn source_detection_validates_rlang_capture_contracts() {
        for helper in ["expr", "quo", "enquo", "enexpr"] {
            let code = format!(r#"rlang::{helper}(!!source("{helper}.R"), unused = 2)"#);
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert!(sources.is_empty(), "{helper}: {sources:?}");
        }

        let malformed = r#"rlang::exprs(!!source("bad.R"), .named = FALSE, .named = TRUE)"#;
        assert!(detect_source_calls(&parse_r(malformed), malformed).is_empty());

        let valid =
            r#"rlang::exprs(.named = { source("control.R"); FALSE }, !!source("operand.R"))"#;
        let sources = detect_source_calls(&parse_r(valid), valid);
        assert_eq!(
            sources
                .into_iter()
                .map(|source| source.path)
                .collect::<Vec<_>>(),
            vec!["control.R", "operand.R"]
        );
    }

    #[test]
    fn source_detection_respects_bquote_splice_control() {
        for code in [
            r#"bquote(..(source("default.R")))"#,
            r#"bquote(..(source("false.R")), splice = FALSE)"#,
            r#"bquote(..(source("unknown.R")), splice = flag)"#,
            r#"bquote(..(source("short-alias.R")), splice = T)"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert!(sources.is_empty(), "{code}: {sources:?}");
        }

        let code = r#"bquote(..(source("root-error.R")), splice = TRUE)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources.is_empty(), "{code}: {sources:?}");

        let code = r#"bquote(list(..(source("nested.R"))), splice = TRUE)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{code}: {sources:?}");
        assert_eq!(sources[0].path, "nested.R");

        for code in [
            r#"bquote(.(source("dot-default.R")))"#,
            r#"bquote(.(source("dot-false.R")), splice = FALSE)"#,
            r#"bquote(.(source("dot-unknown.R")), splice = flag)"#,
            r#"bquote(.(source("dot-lazy.R")), splice = source("not-forced.R"))"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_ne!(sources[0].path, "not-forced.R", "{code}");
        }

        let code = r#"bquote(symbol, splice = source("not-forced.R"))"#;
        assert!(detect_source_calls(&parse_r(code), code).is_empty());

        let code = r#"bquote(list(symbol), splice = source("forced.R"))"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "forced.R");
    }

    #[test]
    fn bquote_source_and_scope_detectors_follow_where_frame() {
        for (code, expected_locality) in [
            (
                r#"bquote(.(source("external.R", local = TRUE)), where = new.env())"#,
                SourceLocality::NonInheriting,
            ),
            (
                r#"bquote(.(source("global.R", local = TRUE)), where = .GlobalEnv)"#,
                SourceLocality::Global,
            ),
            (
                r#"bquote(.(source("global-call.R", local = TRUE)), where = globalenv())"#,
                SourceLocality::Global,
            ),
            (
                r#"bquote(.(source("base-global-call.R", local = TRUE)), where = base::globalenv())"#,
                SourceLocality::Global,
            ),
            (
                r#"bquote(.(source("caller.R", local = TRUE)))"#,
                SourceLocality::CurrentFrame,
            ),
            (
                r#"bquote(where = parent.frame(), expr = .(source("parent.R", local = TRUE)))"#,
                SourceLocality::CurrentFrame,
            ),
            (
                r#"bquote(where = new.env(), expr = .(source("global-default.R")))"#,
                SourceLocality::Global,
            ),
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(
                sources[0].locality, expected_locality,
                "{code}: {sources:?}"
            );
            assert_eq!(
                sources[0].locality != SourceLocality::Global,
                expected_locality != SourceLocality::Global,
                "{code}: {sources:?}"
            );
        }

        let external = r#"bquote(where = new.env(), expr = .({ rm(x); exists("declared") }))"#;
        assert!(detect_rm_calls(&parse_r(external), external).is_empty());
        assert!(detect_exists_calls(&parse_r(external), external).is_empty());

        for where_value in [
            "parent.frame()",
            "environment()",
            ".GlobalEnv",
            "globalenv()",
            "base::globalenv()",
        ] {
            let code = format!(
                "bquote(where = {where_value}, expr = .({{ rm(x); exists(\"declared\") }}))"
            );
            assert_eq!(detect_rm_calls(&parse_r(&code), &code).len(), 1, "{code}");
            assert_eq!(
                detect_exists_calls(&parse_r(&code), &code).len(),
                1,
                "{code}"
            );
        }
    }

    #[test]
    fn nested_bquote_source_locality_composes_capture_frames() {
        let cases = [
            (
                r#"
                    env <- new.env(parent = emptyenv())
                    base::bquote(
                        where = env,
                        expr = .(base::bquote(.(source("external-caller.R", local = TRUE))))
                    )
                "#,
                CaptureEvaluationFrame::ExternalOrUnknown,
                SourceLocality::NonInheriting,
                true,
            ),
            (
                r#"
                    env <- new.env(parent = emptyenv())
                    base::bquote(
                        where = env,
                        expr = .(base::bquote(
                            where = base::globalenv(),
                            expr = .(source("external-global.R", local = TRUE))
                        ))
                    )
                "#,
                CaptureEvaluationFrame::Global,
                SourceLocality::Global,
                true,
            ),
            (
                r#"
                    base::bquote(
                        where = base::globalenv(),
                        expr = .(base::bquote(.(source("global-caller.R", local = TRUE))))
                    )
                "#,
                CaptureEvaluationFrame::Global,
                SourceLocality::Global,
                true,
            ),
            (
                r#"base::bquote(.(base::bquote(.(source("caller-caller.R", local = TRUE)))))"#,
                CaptureEvaluationFrame::Caller,
                SourceLocality::CurrentFrame,
                true,
            ),
            (
                r#"
                    env <- new.env(parent = emptyenv())
                    base::bquote(
                        where = base::globalenv(),
                        expr = .(base::bquote(
                            where = env,
                            expr = .(source("global-external.R", local = TRUE))
                        ))
                    )
                "#,
                CaptureEvaluationFrame::ExternalOrUnknown,
                SourceLocality::NonInheriting,
                true,
            ),
        ];

        for (code, _expected_frame, expected_locality, expected_timeline_contribution) in cases {
            let tree = parse_r(code);
            let mut bindings =
                crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
            let detected = detect_source_calls_with_bindings_and_frames(&tree, code, &mut bindings);
            assert_eq!(detected.len(), 1, "{code}: {detected:?}");
            assert_eq!(
                detected[0].source.locality, expected_locality,
                "{code}: {detected:?}"
            );
            assert_eq!(
                detected[0].contributes_to_timeline(),
                expected_timeline_contribution,
                "{code}: {detected:?}"
            );
        }
    }

    #[test]
    fn nested_external_global_sources_disable_timeline_lending_on_inversion() {
        let code = r#"
            base::bquote(
                where = base::new.env(),
                expr = .(base::bquote(
                    expr = .(source("late.R")),
                    where = { source("early.R"); base::new.env() }
                ))
            )
        "#;
        let tree = parse_r(code);
        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
        let detected = detect_source_calls_with_bindings_and_frames(&tree, code, &mut bindings);

        assert_eq!(
            detected
                .iter()
                .map(|source| source.source.path.as_str())
                .collect::<Vec<_>>(),
            vec!["early.R", "late.R"],
            "capture traversal must retain runtime source order: {detected:?}"
        );
        assert!(
            detected
                .iter()
                .all(|source| source.source.locality == SourceLocality::Global),
            "default source destinations remain global: {detected:?}"
        );
        assert!(
            detected
                .iter()
                .all(|source| !source.contributes_to_timeline()),
            "source-coordinate inversion must suppress global timeline effects: {detected:?}"
        );
    }

    #[test]
    fn nested_external_source_inversion_ignores_inert_and_non_global_effects() {
        let inert = r#"
            base::bquote(
                where = base::new.env(),
                expr = .(base::bquote(
                    expr = .(base::quote(source("late.R"))),
                    where = { source("early.R"); base::new.env() }
                ))
            )
        "#;
        let tree = parse_r(inert);
        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), inert);
        let detected = detect_source_calls_with_bindings_and_frames(&tree, inert, &mut bindings);
        assert_eq!(detected.len(), 1, "{detected:?}");
        assert_eq!(detected[0].source.path, "early.R");
        assert!(detected[0].contributes_to_timeline(), "{detected:?}");

        let non_global = r#"
            base::bquote(
                where = base::new.env(),
                expr = .(base::bquote(
                    expr = .(source("late.R", local = base::new.env())),
                    where = {
                        source("early.R", local = base::new.env())
                        base::new.env()
                    }
                ))
            )
        "#;
        let tree = parse_r(non_global);
        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), non_global);
        let detected =
            detect_source_calls_with_bindings_and_frames(&tree, non_global, &mut bindings);
        assert_eq!(detected.len(), 2, "{detected:?}");
        assert!(
            detected
                .iter()
                .all(|source| source.source.locality == SourceLocality::NonInheriting),
            "{detected:?}"
        );
        assert!(
            detected.iter().all(FramedSource::contributes_to_timeline),
            "explicit non-global destinations must not widen the external-effect predicate: {detected:?}"
        );
    }

    #[test]
    fn external_bquote_function_execution_resets_source_rm_and_exists_frames() {
        let code = r#"
            bquote(
                .(function(
                    default = source("default.R", local = TRUE),
                    declared = exists("default_decl")
                ) {
                    source("body.R", local = TRUE)
                    library(bodypkg)
                    rm(body_name)
                    function() {
                        source("nested.R", local = TRUE)
                        library(nestedpkg)
                        exists("nested_decl")
                    }
                }),
                where = new.env()
            )
        "#;
        let tree = parse_r(code);
        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
        let detected = detect_source_calls_with_bindings_and_frames(&tree, code, &mut bindings);
        assert_eq!(
            detected
                .iter()
                .map(|source| source.source.path.as_str())
                .collect::<Vec<_>>(),
            vec!["default.R", "body.R", "nested.R"]
        );
        assert!(
            detected
                .iter()
                .all(|source| source.source.locality != SourceLocality::Global
                    && source.source.is_function_scoped)
        );
        assert_eq!(detect_rm_calls(&tree, code).len(), 1);
        assert_eq!(detect_exists_calls(&tree, code).len(), 2);
        assert_eq!(
            detect_library_calls(&tree, code)
                .into_iter()
                .map(|call| call.package)
                .collect::<Vec<_>>(),
            vec!["bodypkg", "nestedpkg"]
        );
        assert!(
            extract_attached_packages(code).is_empty(),
            "function-body package loads must remain execution-guarded"
        );

        let direct_external = r#"bquote(.(source("external.R", local = TRUE)), where = new.env())"#;
        let tree = parse_r(direct_external);
        let mut bindings = crate::cross_file::static_path::LazyStaticBindings::new(
            tree.root_node(),
            direct_external,
        );
        let detected =
            detect_source_calls_with_bindings_and_frames(&tree, direct_external, &mut bindings);
        assert_eq!(detected[0].source.locality, SourceLocality::NonInheriting);

        let global_closure = r#"
            bquote(
                .(function() source("global-closure-local.R", local = TRUE)),
                where = .GlobalEnv
            )
        "#;
        let sources = detect_source_calls(&parse_r(global_closure), global_closure);
        assert_eq!(sources.len(), 1);
        assert!(
            sources[0].locality != SourceLocality::Global,
            "function execution must not inherit Global"
        );
    }

    #[test]
    fn bquote_function_syntax_uses_runtime_scope_for_sources_and_packages() {
        let top_level = r#"
            bquote(function() .({
                source("top.R", local = TRUE)
                library(toppkg)
            }))
        "#;
        let top_sources = detect_source_calls(&parse_r(top_level), top_level);
        assert_eq!(top_sources.len(), 1);
        assert!(!top_sources[0].is_function_scoped, "{top_sources:?}");
        assert!(extract_attached_packages(top_level).contains("toppkg"));

        let outer_function = r#"
            outer <- function() {
                bquote(function() .({
                    source("outer.R", local = TRUE)
                    library(outerpkg)
                }))
            }
        "#;
        let outer_sources = detect_source_calls(&parse_r(outer_function), outer_function);
        assert_eq!(outer_sources.len(), 1);
        assert!(outer_sources[0].is_function_scoped, "{outer_sources:?}");
        assert!(!extract_attached_packages(outer_function).contains("outerpkg"));

        let nested_closure = r#"
            bquote(function() .(function() {
                source("nested.R", local = TRUE)
                library(nestedpkg)
            }))
        "#;
        let nested_sources = detect_source_calls(&parse_r(nested_closure), nested_closure);
        assert_eq!(nested_sources.len(), 1);
        assert!(nested_sources[0].is_function_scoped, "{nested_sources:?}");
        assert!(!extract_attached_packages(nested_closure).contains("nestedpkg"));

        let ordinary = r#"
            ordinary <- function() {
                source("ordinary.R", local = TRUE)
                library(ordinarypkg)
            }
        "#;
        let ordinary_sources = detect_source_calls(&parse_r(ordinary), ordinary);
        assert_eq!(ordinary_sources.len(), 1);
        assert!(ordinary_sources[0].is_function_scoped);
        assert!(!extract_attached_packages(ordinary).contains("ordinarypkg"));
    }

    #[test]
    fn source_detection_requires_identifier_bquote_macros() {
        for code in [
            r#"bquote("."(source("literal-dot.R")))"#,
            r#"bquote(list(".."(source("literal-dot-dot.R"))), splice = TRUE)"#,
        ] {
            assert!(
                detect_source_calls(&parse_r(code), code).is_empty(),
                "{code}"
            );
        }

        for (code, expected) in [
            (r#"bquote(`.`(source("backtick-dot.R")))"#, "backtick-dot.R"),
            (
                r#"bquote(list(`..`(source("backtick-dot-dot.R"))), splice = TRUE)"#,
                "backtick-dot-dot.R",
            ),
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(sources[0].path, expected, "{code}");
        }
    }

    #[test]
    fn bquote_package_loads_remain_visible_across_where_frames() {
        let code = r#"
            bquote(
                where = { library(controlpkg); new.env() },
                expr = .({
                    library(operandpkg)
                    libs <- c("fabricatedpkg")
                    sapply(libs, library, character.only = TRUE)
                })
            )
        "#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["controlpkg", "operandpkg"]);
    }

    #[test]
    fn inverted_capture_packages_remain_reachable_but_do_not_lend_to_scope() {
        let code = r#"bquote(
            expr = .(library(dplyr)),
            where = { library(controlpkg); parent.frame() }
        )"#;
        let tree = parse_r(code);
        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
        let reachable: Vec<_> = detect_library_calls_with_bindings(&tree, code, &mut bindings)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(reachable, vec!["dplyr", "controlpkg"]);

        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
        assert!(
            detect_library_calls_with_bindings_for_scope(&tree, code, &mut bindings).is_empty()
        );
    }

    #[test]
    fn inverted_capture_removal_uses_scope_boundary_anchor() {
        let code = "x <- 1\nbquote(expr = .(x), where = { rm(x); parent.frame() })";
        let tree = parse_r(code);
        let public = detect_rm_calls(&tree, code);
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].line, 1);
        assert!(public[0].column > 0);

        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
        let scoped = detect_rm_calls_with_bindings_for_scope(&tree, code, &mut bindings);
        assert_eq!(scoped.len(), 1);
        assert_eq!((scoped[0].call.line, scoped[0].call.column), (1, 0));
    }

    #[test]
    fn static_script_facts_share_one_parse_and_binding_collection() {
        let code = r#"
            path <- "child.R"
            libs <- c("dplyr")
            quote(captured)
            source(path)
            sapply(libs, library, character.only = TRUE)
        "#;
        let before = crate::cross_file::static_path::collection_count_for_current_thread();
        let facts = StaticScriptFacts::from_text(code);
        let after = crate::cross_file::static_path::collection_count_for_current_thread();
        assert_eq!(after, before + 1);
        assert!(facts.top_level_defs.contains("path"));
        assert!(facts.top_level_defs.contains("libs"));
        assert!(facts.attached_packages.contains("dplyr"));
        assert_eq!(facts.source_targets, vec!["child.R"]);
    }

    #[test]
    fn package_detection_requires_identifier_bquote_macros() {
        for code in [
            r#"bquote("."(library(literaldot)))"#,
            r#"bquote(list(".."(library(literaldotdot))), splice = TRUE)"#,
        ] {
            assert!(
                detect_library_calls(&parse_r(code), code).is_empty(),
                "{code}"
            );
        }

        for (code, expected) in [
            (r#"bquote(`.`(library(backtickdot)))"#, "backtickdot"),
            (
                r#"bquote(list(`..`(library(backtickdotdot))), splice = TRUE)"#,
                "backtickdotdot",
            ),
        ] {
            let packages = detect_library_calls(&parse_r(code), code);
            assert_eq!(packages.len(), 1, "{code}: {packages:?}");
            assert_eq!(packages[0].package, expected, "{code}");
        }
    }

    #[test]
    fn source_detection_traverses_nested_dot_inside_disabled_dot_dot() {
        for (code, expected) in [
            (r#"bquote(..(.(source("omitted.R"))))"#, "omitted.R"),
            (
                r#"bquote(list(..(list(.(source("nested-false.R"))))), splice = FALSE)"#,
                "nested-false.R",
            ),
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(sources[0].path, expected, "{code}");
        }
    }

    #[test]
    fn source_detection_ignores_extra_bquote_macro_actuals() {
        for (code, expected) in [
            (
                r#"bquote(.(source("dot-first.R"), source("dot-extra.R")))"#,
                "dot-first.R",
            ),
            (
                r#"bquote(list(..(source("splice-first.R"), source("splice-extra.R"))), splice = TRUE)"#,
                "splice-first.R",
            ),
            (
                r#"bquote(list(.(list(.(source("nested-first.R"))), source("nested-extra.R"))))"#,
                "nested-first.R",
            ),
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(sources[0].path, expected, "{code}");
        }
    }

    #[test]
    fn source_detection_uses_direct_splice_runtime_order_prefix() {
        let code = r#"bquote(list(.(source("head.R")), ..(source("splice.R"))), splice = TRUE)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "splice.R");

        // A direct enabled splice evaluates its operand before the syntactically
        // preceding prefix; a non-vector result aborts before that prefix runs.
        let code = r#"bquote(list(.(source("prefix.R")), ..({ source("operand.R"); function() {} })), splice = TRUE)"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["operand.R"]);
    }

    #[test]
    fn unknown_bquote_splice_suppresses_later_source_effects() {
        let code = r#"base::bquote(list(..(unknown), .(source("tail.R"))), splice = { source("control.R"); flag })"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["control.R"]);
    }

    #[test]
    fn unknown_bquote_splice_extra_actuals_do_not_fabricate_source_edges() {
        let code = r#"base::bquote(list(..(1, .(source("extra.R")))), splice = flag)"#;
        assert!(detect_source_calls(&parse_r(code), code).is_empty());

        let code = r#"base::bquote(list(..(1, .(source("extra.R")))), splice = TRUE)"#;
        assert!(detect_source_calls(&parse_r(code), code).is_empty());

        let code = r#"base::bquote(list(..(1, .(source("extra.R")))), splice = FALSE)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "extra.R");
    }

    #[test]
    fn named_bquote_splice_before_expr_respects_direct_splice_order() {
        let code = r#"base::bquote(splice = flag, expr = list(.(source("prefix.R")), ..(source("operand.R")), .(source("tail.R"))))"#;
        assert!(detect_source_calls(&parse_r(code), code).is_empty());

        let code = r#"base::bquote(splice = { source("control.R"); flag }, expr = list(.(source("prefix.R")), ..(source("operand.R")), .(source("tail.R"))))"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["control.R"]);

        let code = r#"base::bquote(splice = TRUE, expr = list(.(source("prefix.R")), ..(source("operand.R")), .(source("tail.R"))))"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["operand.R"]);

        let code = r#"base::bquote(splice = FALSE, expr = list(.(source("prefix.R")), ..(.(source("operand.R"))), .(source("tail.R"))))"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["prefix.R", "operand.R", "tail.R"]);

        let code = r#"base::bquote(splice = flag, expr = list(.(source("prefix.R")), list(.(source("tail.R")))))"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["prefix.R", "tail.R"]);
    }

    #[test]
    fn bquote_splice_result_gates_later_source_effects() {
        let code = r#"base::bquote(..({ source("operand.R"); function() {} }) + .(source("tail.R")), splice = TRUE)"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["operand.R"]);

        let code = r#"base::bquote(..(unknown) + .(source("tail.R")), splice = TRUE)"#;
        assert!(detect_source_calls(&parse_r(code), code).is_empty());

        for operand in ["1", r#""value""#, "list(1)", "c(1)", "base::c(1)"] {
            let code =
                format!(r#"base::bquote(..({operand}) + .(source("tail.R")), splice = TRUE)"#);
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(sources[0].path, "tail.R", "{code}");
        }

        for setup in [
            "c <- function(...) function() {}",
            "list <- function(...) function() {}",
        ] {
            let helper = setup.split_whitespace().next().unwrap();
            let code = format!(
                "{setup}\nbase::bquote(..({helper}(1)) + .(source(\"tail.R\")), splice = TRUE)"
            );
            assert!(
                detect_source_calls(&parse_r(&code), &code).is_empty(),
                "{code}"
            );
        }
    }

    #[test]
    fn definite_bquote_aborts_stop_source_detection() {
        let code = r#"bquote(list(.(source("head.R")), ..(), .(source("tail.R"))), where = { source("where.R"); parent.frame() }, splice = TRUE)"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["where.R"]);

        let code = r#"bquote(list(.(source("before.R")), list(.(), .(source("inner-tail.R"))), .(source("outer-tail.R"))))"#;
        let paths: Vec<_> = detect_source_calls(&parse_r(code), code)
            .into_iter()
            .map(|source| source.path)
            .collect();
        assert_eq!(paths, vec!["before.R"]);
    }

    #[test]
    fn definite_bquote_aborts_stop_package_detection() {
        let code = r#"bquote(list(.(library(headpkg)), ..(), .(library(tailpkg))), where = { library(wherepkg); parent.frame() }, splice = TRUE)"#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["wherepkg"]);

        let code = r#"bquote(list(.(library(beforepkg)), list(.(), .(library(inner_tail_pkg))), .(library(outer_tail_pkg))))"#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["beforepkg"]);
    }

    #[test]
    fn definite_bquote_aborts_stop_rm_and_exists_detection() {
        let code = r#"bquote(list(.(rm(before)), .(), .(rm(after))))"#;
        let removals = detect_rm_calls(&parse_r(code), code);
        assert_eq!(removals.len(), 1, "{removals:?}");
        assert_eq!(removals[0].symbols, vec!["before"]);

        let code = r#"bquote(list(.(exists("before")), .(), .(exists("after"))))"#;
        let exists: Vec<_> = detect_exists_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.name)
            .collect();
        assert_eq!(exists, vec!["before"]);
    }

    #[test]
    fn source_detection_emits_nothing_when_capture_formal_matching_fails() {
        for code in [
            r#"substitute(expr = source("substitute.R"), expr = x)"#,
            r#"bquote(expr = source("bquote.R"), expr = x)"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert!(sources.is_empty(), "{code}: {sources:?}");
        }
    }

    #[test]
    fn bquote_unknown_splice_conservatively_invalidates_source_bindings() {
        for (capture, preserves) in [
            (
                r#"bquote(..(base::assign("p", "bad.R", envir = .GlobalEnv)))"#,
                true,
            ),
            (
                r#"bquote(..(base::assign("p", "bad.R", envir = .GlobalEnv)), splice = FALSE)"#,
                true,
            ),
            (
                r#"bquote(..(base::assign("p", "bad.R", envir = .GlobalEnv)), splice = flag)"#,
                true,
            ),
            (
                r#"bquote(list(..(base::assign("p", "bad.R", envir = .GlobalEnv))), splice = flag)"#,
                false,
            ),
            (
                r#"bquote(..(base::assign("p", "bad.R", envir = .GlobalEnv)), splice = TRUE)"#,
                true,
            ),
            (
                r#"bquote(list(..(base::assign("p", "bad.R", envir = .GlobalEnv))), splice = TRUE)"#,
                false,
            ),
        ] {
            let code = format!("p <- \"good.R\"\n{capture}\nsource(p)");
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert_eq!(
                sources.iter().any(|source| source.path == "good.R"),
                preserves,
                "{capture}: {sources:?}"
            );
        }
    }

    #[test]
    fn source_detection_does_not_trust_shadowed_or_arbitrary_capture_names() {
        let code = r#"
quote <- function(x) force(x)
quote(source("forced.R"))
"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "got: {sources:?}");
        assert_eq!(sources[0].path, "forced.R");

        let code = r#"other::quote(source("conservative.R"))"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "got: {sources:?}");
        assert_eq!(sources[0].path, "conservative.R");
    }

    #[test]
    fn indirect_unknown_helper_mutations_respect_capture_order_and_scope() {
        let earlier = r#"
x <- get("assign", baseenv())("quote", get("identity", baseenv()), envir = .GlobalEnv)
quote(source("forced.R"))
"#;
        let sources = detect_source_calls(&parse_r(earlier), earlier);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "forced.R");

        let later = r#"
quote(source("captured.R"))
x <- get("assign", baseenv())("quote", get("identity", baseenv()), envir = .GlobalEnv)
"#;
        assert!(
            detect_source_calls(&parse_r(later), later).is_empty(),
            "a later immediate mutation must not affect an earlier use"
        );

        let unrelated_scope = r#"
f <- function() {
  x <- get("assign", baseenv())("quote", get("identity", baseenv()), envir = .GlobalEnv)
}
quote(source("captured.R"))
"#;
        assert!(
            detect_source_calls(&parse_r(unrelated_scope), unrelated_scope).is_empty(),
            "an unrelated function scope must not affect a top-level use"
        );

        let deferred = r#"
f <- function() {
  quote(source("forced.R"))
  x <- get("assign", baseenv())("quote", get("identity", baseenv()), envir = .GlobalEnv)
}
"#;
        let sources = detect_source_calls(&parse_r(deferred), deferred);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert_eq!(sources[0].path, "forced.R");
    }

    #[test]
    fn indirect_unknown_helper_mutation_exposes_quoted_package_effects() {
        let code = r#"
x <- get("assign", baseenv())("quote", get("identity", baseenv()), envir = .GlobalEnv)
quote(library(dplyr))
"#;
        let calls = detect_library_calls(&parse_r(code), code);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert!(extract_attached_packages(code).contains("dplyr"));
    }

    #[test]
    fn evaluated_capture_mutations_invalidate_static_source_bindings() {
        for evaluated in [
            r#"bquote(.(base::assign("p", "bad.R", envir = .GlobalEnv)))"#,
            r#"substitute(x, env = base::assign("p", "bad.R", envir = .GlobalEnv))"#,
            r#"quote <- function(x) force(x)
quote(base::assign("p", "bad.R", envir = .GlobalEnv))"#,
        ] {
            let code = format!("p <- \"good.R\"\n{evaluated}\nsource(p)");
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert!(sources.is_empty(), "{evaluated}: {sources:?}");
        }

        let code = r#"
p <- "good.R"
quote(base::assign("p", "bad.R", envir = .GlobalEnv))
source(p)
"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "got: {sources:?}");
        assert_eq!(sources[0].path, "good.R");
    }

    #[test]
    fn test_source_named_argument() {
        let code = r#"source(file = "utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
    }

    #[test]
    fn test_source_backtick_named_arguments() {
        for code in [
            r#"source(`file` = "utils.R", `local` = FALSE)"#,
            r#"source("file" = "utils.R", "local" = FALSE)"#,
            r#"source(r"(file)" = "utils.R", r"(local)" = FALSE)"#,
        ] {
            let tree = parse_r(code);
            let sources = detect_source_calls(&tree, code);
            assert_eq!(sources.len(), 1, "{code}");
            assert_eq!(sources[0].path, "utils.R");
            assert!(sources[0].locality == SourceLocality::Global);
            assert!(sources[0].inherits_symbols());
        }

        let code = r#"sys.source(`file` = "utils.R", `envir` = globalenv())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].locality == SourceLocality::Global);
    }

    #[test]
    fn test_sys_source() {
        let code = r#"sys.source("utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
        assert!(sources[0].is_sys_source);
    }

    #[test]
    fn test_source_with_local_true() {
        for code in [
            r#"source("utils.R", local = TRUE)"#,
            r#"source("utils.R", local = T)"#,
            r#"source("utils.R", TRUE)"#,
        ] {
            let tree = parse_r(code);
            let sources = detect_source_calls(&tree, code);
            assert_eq!(sources.len(), 1, "{code}");
            assert!(sources[0].locality != SourceLocality::Global, "{code}");
            assert_eq!(sources[0].locality, SourceLocality::CurrentFrame, "{code}");
        }
    }

    #[test]
    fn test_source_with_unknown_local_is_conservatively_non_inheriting() {
        for code in [
            r#"source("utils.R", local = flag)"#,
            r#"source("utils.R", local = new.env())"#,
            r#"source("utils.R", flag)"#,
            r#"source("utils.R", loc = flag)"#,
        ] {
            let tree = parse_r(code);
            let sources = detect_source_calls(&tree, code);
            assert_eq!(sources.len(), 1, "{code}");
            assert!(sources[0].locality != SourceLocality::Global, "{code}");
            assert_eq!(sources[0].locality, SourceLocality::NonInheriting, "{code}");
            assert!(!sources[0].inherits_symbols(), "{code}");
        }

        for code in [
            r#"source("utils.R")"#,
            r#"source("utils.R", local = FALSE)"#,
            r#"source("utils.R", local = F)"#,
            r#"source("utils.R", local = .GlobalEnv)"#,
            r#"source("utils.R", local = globalenv())"#,
            r#"source("utils.R", local = base::globalenv())"#,
            r#"source("utils.R", FALSE)"#,
            r#"source("utils.R", loc = FALSE)"#,
            r#"source("utils.R", local = )"#,
            r#"source("utils.R", , echo = FALSE)"#,
            r#"source(f = "utils.R", local = FALSE)"#,
        ] {
            let tree = parse_r(code);
            let sources = detect_source_calls(&tree, code);
            assert_eq!(sources.len(), 1, "{code}");
            assert_eq!(sources[0].path, "utils.R", "{code}");
            assert!(sources[0].locality == SourceLocality::Global, "{code}");
            assert_eq!(sources[0].locality, SourceLocality::Global, "{code}");
            assert!(sources[0].inherits_symbols(), "{code}");
        }
    }

    #[test]
    fn global_capture_promotes_only_proven_current_frame_locality() {
        let code = r#"
            bquote(where = .GlobalEnv, expr = .(source("external.R", local = e, chdir = TRUE)))
            bquote(where = .GlobalEnv, expr = .(source("true.R", local = TRUE)))
            bquote(where = .GlobalEnv, expr = .(source("short-true.R", local = T)))
            bquote(where = .GlobalEnv, expr = .(source("false.R", local = FALSE)))
            bquote(where = .GlobalEnv, expr = .(source("global-env.R", local = base::globalenv())))
        "#;
        let tree = parse_r(code);
        let mut bindings =
            crate::cross_file::static_path::LazyStaticBindings::new(tree.root_node(), code);
        let detected = detect_source_calls_with_bindings_and_frames(&tree, code, &mut bindings);
        assert_eq!(detected.len(), 5, "{detected:?}");

        let external = &detected[0];
        assert_eq!(external.source.path, "external.R");
        assert_eq!(external.source.locality, SourceLocality::NonInheriting);
        assert!(external.source.locality != SourceLocality::Global);
        assert!(external.source.chdir, "chdir metadata must be preserved");
        assert!(external.contributes_to_timeline());

        for source in &detected[1..] {
            assert_eq!(source.source.locality, SourceLocality::Global, "{source:?}");
            assert!(
                source.source.locality == SourceLocality::Global,
                "{source:?}"
            );
            assert!(source.contributes_to_timeline(), "{source:?}");
        }

        assert_eq!(
            StaticScriptFacts::from_text(code).source_targets,
            vec![
                "true.R".to_string(),
                "short-true.R".to_string(),
                "false.R".to_string(),
                "global-env.R".to_string(),
            ]
        );
    }

    #[test]
    fn test_invalid_or_ambiguous_source_argument_matching_is_skipped() {
        for code in [
            r#"source("utils.R", local = FALSE, local = FALSE)"#,
            r#"source(, "utils.R", local = FALSE)"#,
            r#"sys.source(, "utils.R", envir = globalenv())"#,
            r#"sys.source("utils.R", envir = globalenv(), envir = globalenv())"#,
        ] {
            let tree = parse_r(code);
            assert!(detect_source_calls(&tree, code).is_empty(), "{code}");
        }
    }

    #[test]
    fn test_partial_file_name_with_positional_arguments_uses_r_matching() {
        for (code, path) in [
            (r#"source(f = "child.R", FALSE)"#, "child.R"),
            (
                r#"source(f = "real.R", "value-for-echo", local = FALSE)"#,
                "real.R",
            ),
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(sources[0].path, path, "{code}");
            assert!(sources[0].locality == SourceLocality::Global, "{code}");
        }
    }

    #[test]
    fn incomplete_source_calls_remain_strictly_ignored_by_detection() {
        for code in [
            r#"source("uti"#,
            r#"source("utils.R""#,
            r#"source(f = "uti"#,
        ] {
            assert!(
                detect_source_calls(&parse_r(code), code).is_empty(),
                "{code}"
            );
        }
    }

    #[test]
    fn test_source_with_chdir_true() {
        for code in [
            r#"source("utils.R", chdir = TRUE)"#,
            r#"source("utils.R", chdir = T)"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}");
            assert!(sources[0].chdir, "{code}");
        }

        for code in [
            r#"source("utils.R", chdir = FALSE)"#,
            r#"source("utils.R", chdir = F)"#,
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}");
            assert!(!sources[0].chdir, "{code}");
        }
    }

    #[test]
    fn test_source_with_positional_chdir_true() {
        let code = r#"source("utils.R", FALSE, FALSE, FALSE, NULL, FALSE, FALSE, "", 60, 60, "keepInteger", TRUE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].locality == SourceLocality::Global);
        assert!(sources[0].chdir);

        let code = r#"sys.source("utils.R", globalenv(), TRUE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].chdir);
        assert!(sources[0].locality == SourceLocality::Global);
        assert!(sources[0].inherits_symbols());
    }

    #[test]
    fn test_source_with_variable_path_skipped() {
        let code = "source(my_path)";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_with_paste0_skipped() {
        let code = r#"source(paste0("dir/", filename))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_multiple_source_calls() {
        let code = r#"source("a.R")
source("b.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path, "a.R");
        assert_eq!(sources[0].line, 0);
        assert_eq!(sources[1].path, "b.R");
        assert_eq!(sources[1].line, 1);
    }

    #[test]
    fn test_source_position() {
        let code = "x <- 1\nsource(\"utils.R\")";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].line, 1);
        assert_eq!(sources[0].column, 0);
    }

    /// Regression coverage for issue #138: source() at top level must be
    /// flagged as not function-scoped.
    #[test]
    fn test_source_top_level_is_not_function_scoped() {
        let code = "source(\"utils.R\")\n";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(!sources[0].is_function_scoped);
    }

    /// Regression coverage for issue #138: source() inside a function body
    /// is deferred-evaluation and must be flagged as function-scoped.
    #[test]
    fn test_source_in_function_body_is_function_scoped() {
        let code = "f <- function() {\n  source(\"utils.R\")\n}\n";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_function_scoped);
        assert_eq!(sources[0].locality, SourceLocality::Global);
    }

    /// Regression coverage for issue #138: walking ancestors must catch
    /// source() inside nested function bodies, not just the immediate
    /// parent function. Also covers R 4.1+ lambda syntax (`\(...)`),
    /// which tree-sitter-r normalizes to `function_definition`.
    #[test]
    fn test_source_in_nested_function_body_is_function_scoped() {
        let code = "outer <- function() {\n  inner <- \\() {\n    source(\"utils.R\")\n  }\n}\n";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_function_scoped);
    }

    #[test]
    fn function_scoped_sources_preserve_current_frame_vs_non_inheriting() {
        let code = r#"
            outer <- function() {
                source("current.R", local = TRUE)
                inner <- function() source("unknown.R", local = new.env(emptyenv()))
            }
        "#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 2, "{sources:?}");
        assert!(sources.iter().all(|source| source.is_function_scoped));
        assert_eq!(sources[0].locality, SourceLocality::CurrentFrame);
        assert_eq!(sources[1].locality, SourceLocality::NonInheriting);
    }

    #[test]
    fn static_script_facts_source_targets_apply_all_scope_filters() {
        let code = r#"
source("global.R")
source("false.R", local = FALSE)
source(f = "partial-file.R", local = FALSE)
source("short-false.R", local = F)
source(file.path("computed", "helper.R"))
source("local.R", local = TRUE)
source("short-local.R", local = T)
source("dynamic-local.R", local = flag)
source("environment-local.R", local = new.env())
source("positional-local.R", flag)
source("partial-local.R", loc = flag)
source("missing-local.R", local = )
source("duplicate-local.R", local = FALSE, local = FALSE)
source(, "missing-file.R", local = FALSE)
sys.source("base.R")
sys.source(, "missing-sys-file.R", envir = globalenv())
sys.source("duplicate-sys-envir.R", envir = globalenv(), envir = globalenv())
f <- function() source("deferred.R")
"#;
        assert_eq!(
            StaticScriptFacts::from_text(code).source_targets,
            vec![
                "global.R",
                "false.R",
                "partial-file.R",
                "short-false.R",
                "computed/helper.R",
                "missing-local.R"
            ]
        );
    }

    #[test]
    fn static_script_facts_source_targets_respect_capture_locality_and_runtime_order() {
        let code = r#"
            bquote(where = .GlobalEnv, expr = .(source("captured-environment.R", local = e)))
            bquote(where = .GlobalEnv, expr = .(source("captured-current.R", local = TRUE)))
            bquote(where = { source("captured-orderable.R"); parent.frame() }, expr = .(NULL))
            bquote(expr = .(rm(x)), where = { source("captured-inverted.R"); parent.frame() })
        "#;
        assert_eq!(
            StaticScriptFacts::from_text(code).source_targets,
            vec![
                "captured-current.R".to_string(),
                "captured-orderable.R".to_string(),
            ]
        );
        assert_eq!(
            detect_source_calls(&parse_r(code), code)
                .into_iter()
                .map(|source| source.path)
                .collect::<Vec<_>>(),
            vec![
                "captured-environment.R",
                "captured-current.R",
                "captured-orderable.R",
                "captured-inverted.R",
            ],
            "dependency-oriented detection must retain every executed source"
        );
    }

    #[test]
    fn test_non_source_call_ignored() {
        let code = "print(\"hello\")";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_sys_source_with_globalenv() {
        for code in [
            r#"sys.source("utils.R", envir = globalenv())"#,
            "result <- helper(1)\nsys.source(\"utils.R\", envir = globalenv())",
        ] {
            let tree = parse_r(code);
            let sources = detect_source_calls(&tree, code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert!(sources[0].is_sys_source);
            assert!(sources[0].locality == SourceLocality::Global, "{code}");
            assert!(sources[0].inherits_symbols(), "{code}");
        }
    }

    #[test]
    fn test_sys_source_with_global_env_dot() {
        let code = r#"sys.source("utils.R", envir = .GlobalEnv)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].locality == SourceLocality::Global);
        assert!(sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_shadowed_global_environment_is_not_global() {
        for code in [
            ".GlobalEnv <- new.env()\nsys.source(\"utils.R\", envir = .GlobalEnv)",
            "globalenv <- function() new.env()\nsys.source(\"utils.R\", envir = globalenv())",
            "name <- \"globalenv\"\nassign(name, function() new.env())\nsys.source(\"utils.R\", envir = globalenv())",
        ] {
            let tree = parse_r(code);
            let sources = detect_source_calls(&tree, code);
            assert_eq!(sources.len(), 1);
            assert!(sources[0].locality != SourceLocality::Global);
            assert!(!sources[0].inherits_symbols());
        }

        let code = r#"later <- function(x) function() x
g <- later(sys.source("utils.R", envir = globalenv()))
globalenv <- function() new.env()
g()"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].locality != SourceLocality::Global);
    }

    #[test]
    fn unrelated_function_bindings_do_not_shadow_top_level_source_aliases() {
        let code = r#"
f <- function(globalenv, T) {
  .GlobalEnv <- new.env()
  F <- TRUE
}
sys.source("sys.R", envir = globalenv())
source("local.R", local = F, chdir = T)
"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 2, "{sources:?}");
        assert!(sources[0].locality == SourceLocality::Global);
        assert!(sources[1].locality == SourceLocality::Global);
        assert!(sources[1].chdir);
    }

    #[test]
    fn relevant_scope_bindings_keep_aliases_conservative() {
        for code in [
            "T <- FALSE\nsource(\"utils.R\", local = T)",
            "F <- TRUE\nsource(\"utils.R\", local = F)",
            ".GlobalEnv <- new.env()\nsource(\"utils.R\", local = .GlobalEnv)",
            "globalenv <- function() new.env()\nsource(\"utils.R\", local = globalenv())",
            "mutate <- function() assign(\"F\", TRUE, envir = .GlobalEnv)\nmutate()\nsource(\"utils.R\", local = F)",
            "mutate <- function() F <<- TRUE\nmutate()\nsource(\"utils.R\", local = F)",
            r#"mutate <- function() `\x46` <<- TRUE
mutate()
source("utils.R", local = F)"#,
            "f <- function(T) source(\"utils.R\", local = T)",
            "f <- function() { x <- helper(); source(\"utils.R\", local = F) }",
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert!(sources[0].locality != SourceLocality::Global, "{code}");
        }

        let code = "mutate <- function() assign(\"globalenv\", function() new.env(), envir = .GlobalEnv)\nmutate()\nsys.source(\"utils.R\", envir = globalenv())";
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].locality != SourceLocality::Global);

        // Deferred calls consider later bindings and mutation uncertainty.
        let code = "f <- function() {\n  sys.source(\"utils.R\", envir = globalenv())\n  globalenv <- function() new.env()\n}";
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].locality != SourceLocality::Global);
    }

    #[test]
    fn trusted_immediate_removals_restore_base_aliases() {
        let code = "F <- TRUE\nrm(F)\nsource(\"utils.R\", local = F)";
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources[0].locality == SourceLocality::Global);

        let code = "T <- FALSE\nrm(T)\nsource(\"utils.R\", chdir = T)";
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources[0].chdir);

        let code = "globalenv <- function() new.env()\nrm(globalenv)\nsys.source(\"utils.R\", envir = globalenv())";
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources[0].locality == SourceLocality::Global);

        let code = ".GlobalEnv <- NULL\nrm(.GlobalEnv)\nsource(\"utils.R\", local = .GlobalEnv)";
        let sources = detect_source_calls(&parse_r(code), code);
        assert!(sources[0].locality == SourceLocality::Global);

        for code in [
            "F <- TRUE\nf <- function() rm(F)\nsource(\"utils.R\", local = F)",
            "F <- TRUE\nrm(F, envir = other)\nsource(\"utils.R\", local = F)",
        ] {
            let sources = detect_source_calls(&parse_r(code), code);
            assert!(sources[0].locality != SourceLocality::Global, "{code}");
        }
    }

    #[test]
    fn unknown_bquote_splice_branch_removals_do_not_restore_local_aliases() {
        for (splice, expected_local) in [("TRUE", false), ("FALSE", true), ("flag", true)] {
            let code = format!(
                "F <- TRUE\nbase::bquote(list(..(base::rm(F))), splice = {splice})\nsource(\"utils.R\", local = F)"
            );
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(
                sources[0].locality != SourceLocality::Global,
                expected_local,
                "{code}: {sources:?}"
            );
        }

        for splice in ["TRUE", "FALSE", "flag"] {
            let code = format!(
                "F <- FALSE\nbase::bquote(list(.(F <- TRUE), ..(base::rm(F))), splice = {splice})\nsource(\"utils.R\", local = F)"
            );
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert!(
                sources[0].locality != SourceLocality::Global,
                "{code}: {sources:?}"
            );
        }
    }

    #[test]
    fn cross_certainty_capture_inversion_keeps_source_local_conservative() {
        for (splice, expected_local) in [("TRUE", false), ("FALSE", true), ("flag", true)] {
            let code = format!(
                r#"
base::bquote(
  where = {{ rm(F); .GlobalEnv }},
  expr = list(..(1, .(F <- TRUE))),
  splice = {splice}
)
source("child.R", local = F)
"#
            );
            let sources = detect_source_calls(&parse_r(&code), &code);
            assert_eq!(sources.len(), 1, "{code}: {sources:?}");
            assert_eq!(
                sources[0].locality != SourceLocality::Global,
                expected_local,
                "{code}: {sources:?}"
            );
            assert_eq!(
                sources[0].inherits_symbols(),
                !expected_local,
                "{code}: {sources:?}"
            );
        }
    }

    #[test]
    fn escaped_parameters_shadow_aliases_only_in_their_function_scope() {
        let code = r#"f <- function(`\x46`) source("utils.R", local = F)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].locality != SourceLocality::Global);

        let code = r#"f <- function(`\x54`) source("utils.R", chdir = T)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(!sources[0].chdir);

        let code = r#"f <- function(`.Global\x45nv`) source("utils.R", local = .GlobalEnv)"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].locality != SourceLocality::Global);

        let code = r#"f <- function(`global\x65nv`) sys.source("utils.R", envir = globalenv())"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 1, "{sources:?}");
        assert!(sources[0].locality != SourceLocality::Global);
    }

    #[test]
    fn later_immediate_unknown_mutation_does_not_retroactively_shadow_aliases() {
        let code = r#"
source("local.R", local = F, chdir = T)
sys.source("sys.R", envir = globalenv())
x <- get("assign", baseenv())("F", TRUE, envir = .GlobalEnv)
"#;
        let sources = detect_source_calls(&parse_r(code), code);
        assert_eq!(sources.len(), 2, "{sources:?}");
        assert!(sources[0].locality == SourceLocality::Global);
        assert!(sources[0].chdir);
        assert!(sources[1].locality == SourceLocality::Global);
    }

    #[test]
    fn test_sys_source_with_new_env() {
        let code = r#"sys.source("utils.R", envir = new.env())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].locality != SourceLocality::Global);
        assert_eq!(sources[0].locality, SourceLocality::NonInheriting);
        assert!(!sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_without_envir() {
        // sys.source without envir defaults to baseenv(), not global
        let code = r#"sys.source("utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].locality != SourceLocality::Global);
        assert_eq!(sources[0].locality, SourceLocality::NonInheriting);
        assert!(!sources[0].inherits_symbols());
    }

    // ==================== rm()/remove() detection tests ====================

    #[test]
    fn test_rm_single_bare_symbol() {
        let code = "rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[0].column, 0);
    }

    #[test]
    fn test_rm_multiple_bare_symbols() {
        let code = "rm(x, y, z)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y", "z"]);
    }

    #[test]
    fn test_remove_single_bare_symbol() {
        let code = "remove(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_remove_multiple_bare_symbols() {
        let code = "remove(a, b, c)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b", "c"]);
    }
    #[test]
    fn test_internal_remove_routine_call_ignored() {
        // In base/R/rm.R, `.Internal(remove(list, envir, inherits))`
        // names the C entry point used to implement rm()/remove(); it is
        // not an R-level `remove()` call and must not remove `envir` from
        // the surrounding function scope.
        let code = ".Internal(remove(list, envir, inherits))";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_empty_call() {
        // rm() with no arguments should not produce any RmCall
        let code = "rm()";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_string_argument_skipped() {
        // rm("x") with string in positional arg should be skipped (not a bare symbol)
        let code = r#"rm("x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_expression_argument_skipped() {
        // rm(x + y) should be skipped (not an identifier)
        let code = "rm(x + y)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_number_argument_skipped() {
        // rm(1) should be skipped (not an identifier)
        let code = "rm(1)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_position() {
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].line, 1);
        assert_eq!(rm_calls[0].column, 0);
    }

    #[test]
    fn test_rm_position_with_offset() {
        let code = "x <- 1; rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[0].column, 8);
    }

    #[test]
    fn test_multiple_rm_calls() {
        let code = "rm(x)\nrm(y, z)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 2);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[1].symbols, vec!["y", "z"]);
        assert_eq!(rm_calls[1].line, 1);
    }

    #[test]
    fn test_non_rm_call_ignored() {
        let code = "print(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_list_single_string() {
        // rm(list = "x") should extract "x"
        let code = r#"rm(list = "x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_mixed_bare_and_list() {
        // rm(x, list = "y") - should extract both bare symbol x and list symbol y
        let code = r#"rm(x, list = "y")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y"]);
    }

    // ------------------------------------------------------------------
    // detect_exists_calls: `exists("name")` declares `name` (parity with
    // `# raven: var name`).
    // ------------------------------------------------------------------

    #[test]
    fn test_exists_double_quoted_name() {
        let code = r#"exists("apple")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
        assert_eq!(calls[0].line, 0);
    }

    #[test]
    fn test_exists_single_quoted_name() {
        let code = "exists('apple')";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_negated_guard() {
        // The idiomatic `if (!exists("x")) x <- ...` guard.
        let code = "if (!exists(\"apple\")) apple <- 1";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_named_x_argument() {
        let code = r#"exists(x = "apple")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_extra_args_still_declares() {
        // `where=`/`inherits=` do not change that the user named `apple`.
        let code = r#"exists("apple", inherits = FALSE)"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "apple");
    }

    #[test]
    fn test_exists_non_literal_argument_skipped() {
        // A variable name (not a string literal) is not statically determinable.
        let code = "exists(varname)";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_computed_argument_skipped() {
        let code = r#"exists(paste0("ap", "ple"))"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_empty_call_skipped() {
        let code = "exists()";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_empty_string_skipped() {
        // `exists("")` names nothing usable — declare nothing (parity with the
        // `# raven: var` directive, which also skips an empty name).
        let code = r#"exists("")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_whitespace_string_skipped() {
        // A whitespace-only name is also skipped, matching `# raven: var`'s
        // `name.trim().is_empty()` rule.
        let code = r#"exists("   ")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_exists_non_syntactic_name_extracted_raw() {
        // The detector returns the RAW string contents (`my var`); call-site
        // backtick-wrapping (`` `my var` ``) happens later, during declared-symbol
        // synthesis in `scope.rs` (via `callee_name_for_match`), so it matches a
        // `` `my var` `` use.
        let code = r#"exists("my var")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "my var");
    }

    #[test]
    fn test_exists_multiple_calls() {
        let code = "exists(\"a\")\nexists(\"b\")";
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].line, 0);
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].line, 1);
    }

    #[test]
    fn capture_detectors_ignore_extra_bquote_macro_actuals() {
        for code in [
            r#"bquote(.(exists("kept"), exists("extra")))"#,
            r#"bquote(list(..(exists("kept"), exists("extra"))), splice = TRUE)"#,
        ] {
            let calls = detect_exists_calls(&parse_r(code), code);
            assert_eq!(calls.len(), 1, "{code}: {calls:?}");
            assert_eq!(calls[0].name, "kept", "{code}");
        }

        for code in [
            r#"bquote(.(rm(kept), rm(extra)))"#,
            r#"bquote(list(..(rm(kept), rm(extra))), splice = TRUE)"#,
        ] {
            let calls = detect_rm_calls(&parse_r(code), code);
            assert_eq!(calls.len(), 1, "{code}: {calls:?}");
            assert_eq!(calls[0].symbols, vec!["kept"], "{code}");
        }
    }

    #[test]
    fn test_non_exists_call_ignored() {
        let code = r#"file.exists("apple")"#;
        let tree = parse_r(code);
        let calls = detect_exists_calls(&tree, code);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_rm_inside_function() {
        let code = "f <- function() { rm(x) }";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_utf16_column() {
        // Test with emoji before rm() to verify UTF-16 column calculation
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let code = "🎉; rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        // Column should be 2 (emoji) + 2 ("; ") = 4 in UTF-16
        assert_eq!(rm_calls[0].column, 4);
    }

    // ==================== list= argument parsing tests ====================

    #[test]
    fn test_rm_list_single_string_double_quotes() {
        let code = r#"rm(list = "myvar")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["myvar"]);
    }

    #[test]
    fn test_rm_list_single_string_single_quotes() {
        let code = "rm(list = 'myvar')";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["myvar"]);
    }

    #[test]
    fn test_rm_list_c_multiple_strings() {
        let code = r#"rm(list = c("a", "b", "c"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_rm_list_c_single_string() {
        let code = r#"rm(list = c("x"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_list_c_empty() {
        let code = "rm(list = c())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // Empty c() produces no symbols, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_variable_skipped() {
        // rm(list = var) - variable reference should be skipped
        let code = "rm(list = my_var)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_ls_skipped() {
        // rm(list = ls()) - function call should be skipped
        let code = "rm(list = ls())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_ls_pattern_skipped() {
        // rm(list = ls(pattern = "^tmp")) - function call should be skipped
        let code = r#"rm(list = ls(pattern = "^tmp"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_paste0_skipped() {
        // rm(list = paste0("x", 1:3)) - function call should be skipped
        let code = r#"rm(list = paste0("x", 1:3))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_c_with_mixed_args() {
        // c() with mixed string and non-string args - only extract strings
        let code = r#"rm(list = c("a", var, "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        // Only string literals are extracted
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_rm_bare_and_list_combined() {
        // rm(x, y, list = c("a", "b")) - should extract all symbols
        let code = r#"rm(x, y, list = c("a", "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y", "a", "b"]);
    }

    #[test]
    fn test_remove_list_single_string() {
        // remove() should work the same as rm()
        let code = r#"remove(list = "x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_remove_list_c_multiple() {
        let code = r#"remove(list = c("a", "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_rm_list_number_skipped() {
        // rm(list = 123) - number should be skipped
        let code = "rm(list = 123)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_expression_skipped() {
        // rm(list = x + y) - expression should be skipped
        let code = "rm(list = x + y)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    // ==================== envir= argument filtering tests ====================

    #[test]
    fn test_rm_without_envir_processed() {
        // rm(x) without envir= should be processed normally
        let code = "rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_globalenv_processed() {
        // rm(x, envir = globalenv()) should be processed normally
        let code = "rm(x, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_dot_globalenv_processed() {
        // rm(x, envir = .GlobalEnv) should be processed normally
        let code = "rm(x, envir = .GlobalEnv)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_custom_skipped() {
        // rm(x, envir = my_env) should be skipped (non-default environment)
        let code = "rm(x, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_new_env_skipped() {
        // rm(x, envir = new.env()) should be skipped
        let code = "rm(x, envir = new.env())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_parent_frame_skipped() {
        // rm(x, envir = parent.frame()) should be skipped
        let code = "rm(x, envir = parent.frame())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_baseenv_skipped() {
        // rm(x, envir = baseenv()) should be skipped
        let code = "rm(x, envir = baseenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_multiple_symbols_with_envir_custom_skipped() {
        // rm(x, y, z, envir = my_env) should be skipped entirely
        let code = "rm(x, y, z, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_with_envir_custom_skipped() {
        // rm(list = c("a", "b"), envir = my_env) should be skipped
        let code = r#"rm(list = c("a", "b"), envir = my_env)"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_with_envir_globalenv_processed() {
        // rm(list = c("a", "b"), envir = globalenv()) should be processed
        let code = r#"rm(list = c("a", "b"), envir = globalenv())"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_remove_with_envir_custom_skipped() {
        // remove(x, envir = my_env) should be skipped
        let code = "remove(x, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_remove_with_envir_globalenv_processed() {
        // remove(x, envir = globalenv()) should be processed
        let code = "remove(x, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_mixed_with_envir_globalenv_processed() {
        // rm(x, list = "y", envir = .GlobalEnv) should be processed
        let code = r#"rm(x, list = "y", envir = .GlobalEnv)"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y"]);
    }

    #[test]
    fn test_multiple_rm_calls_with_different_envir() {
        // Mix of rm() calls with different envir= values
        let code = "rm(a)\nrm(b, envir = my_env)\nrm(c, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // Only rm(a) and rm(c, envir = globalenv()) should be detected
        assert_eq!(rm_calls.len(), 2);
        assert_eq!(rm_calls[0].symbols, vec!["a"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[1].symbols, vec!["c"]);
        assert_eq!(rm_calls[1].line, 2);
    }

    // ==================== error/missing AST node tests ====================

    #[test]
    fn test_rm_malformed_empty_arg_skipped() {
        // rm(,) - malformed with missing argument
        let code = "rm(,)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_malformed_list_missing_value_skipped() {
        // rm(list = ) - malformed with missing value
        let code = "rm(list = )";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    // ==================== library()/require()/loadNamespace() detection tests ====================

    #[test]
    fn test_library_bare_identifier() {
        // library(dplyr) - bare identifier
        // Validates: Requirement 1.1
        let code = "library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[0].line, 0);
    }

    #[test]
    fn test_library_double_quoted_string() {
        // library("dplyr") - double-quoted string
        // Validates: Requirement 1.2
        let code = r#"library("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_single_quoted_string() {
        // library('dplyr') - single-quoted string
        // Validates: Requirement 1.3
        let code = "library('dplyr')";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_bare_identifier() {
        // require(dplyr) - bare identifier
        // Validates: Requirement 1.4
        let code = "require(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_quoted_string() {
        // require("dplyr") - quoted string
        // Validates: Requirement 1.4
        let code = r#"require("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_load_namespace_quoted_string() {
        // loadNamespace("dplyr") - quoted string
        // Validates: Requirement 1.5
        let code = r#"loadNamespace("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_load_namespace_bare_identifier() {
        // loadNamespace(dplyr) - bare identifier
        // Validates: Requirement 1.5
        let code = "loadNamespace(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_attaches_flag_distinguishes_library_from_load_namespace() {
        // `library`/`require` attach (bare names become available); `loadNamespace`
        // only loads the namespace for qualified `pkg::` access. The `attaches`
        // flag must reflect that distinction — see `LibraryCall::attaches`.
        for (code, expected) in [
            ("library(shiny)", true),
            ("require(shiny)", true),
            (r#"loadNamespace("shiny")"#, false),
            ("loadNamespace(shiny)", false),
        ] {
            let tree = parse_r(code);
            let lib_calls = detect_library_calls(&tree, code);
            assert_eq!(lib_calls.len(), 1, "exactly one call detected for `{code}`");
            assert_eq!(lib_calls[0].package, "shiny", "package name for `{code}`");
            assert_eq!(
                lib_calls[0].attaches, expected,
                "attaches flag for `{code}`"
            );
        }
    }

    #[test]
    fn test_library_variable_skipped() {
        // library(pkg_name) where pkg_name is a variable - should be skipped
        // Validates: Requirement 1.6
        // Note: We can't distinguish a variable from a bare package name statically,
        // so this test verifies that we DO detect it (as we treat all identifiers as package names)
        let code = "pkg_name <- 'dplyr'\nlibrary(pkg_name)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        // We detect it because we can't distinguish variable from package name
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "pkg_name");
    }

    #[test]
    fn test_library_expression_skipped() {
        // library(paste0("dp", "lyr")) - expression should be skipped
        // Validates: Requirement 1.6
        let code = r#"library(paste0("dp", "lyr"))"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_true_skipped() {
        // library("dplyr", character.only = TRUE) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"library("dplyr", character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_t_skipped() {
        // library("dplyr", character.only = T) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"library("dplyr", character.only = T)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_false_processed() {
        // library("dplyr", character.only = FALSE) - should be processed
        let code = r#"library("dplyr", character.only = FALSE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_character_only_true_skipped() {
        // require("dplyr", character.only = TRUE) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"require("dplyr", character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_multiple_library_calls() {
        // Multiple library calls in document order
        // Validates: Requirement 1.8
        let code = "library(dplyr)\nlibrary(ggplot2)\nrequire(tidyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 3);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[0].line, 0);
        assert_eq!(lib_calls[1].package, "ggplot2");
        assert_eq!(lib_calls[1].line, 1);
        assert_eq!(lib_calls[2].package, "tidyr");
        assert_eq!(lib_calls[2].line, 2);
    }

    #[test]
    fn test_library_named_package_argument() {
        // library(package = dplyr) - named argument
        let code = "library(package = dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_named_package_argument_quoted() {
        // library(package = "dplyr") - named argument with string
        let code = r#"library(package = "dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_position() {
        // Test position tracking
        let code = "x <- 1\nlibrary(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].line, 1);
        // Column should be at end of call
        assert_eq!(lib_calls[0].column, 14); // "library(dplyr)" is 14 chars
    }

    #[test]
    fn test_library_position_with_offset() {
        // Test position with offset on same line
        let code = "x <- 1; library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].line, 0);
        // Column should be at end of call: "x <- 1; library(dplyr)" = 22 chars
        assert_eq!(lib_calls[0].column, 22);
    }

    #[test]
    fn test_library_with_utf16_column() {
        // Test with emoji before library() to verify UTF-16 column calculation
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let code = "🎉; library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        // Column should be: 2 (emoji) + 2 ("; ") + 14 ("library(dplyr)") = 18 in UTF-16
        assert_eq!(lib_calls[0].column, 18);
    }

    #[test]
    fn test_library_inside_function() {
        // library() inside a function body
        let code = "f <- function() { library(dplyr) }";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        // function_scope is None for now (will be populated in task 6.2)
        assert!(lib_calls[0].function_scope.is_none());
    }

    #[test]
    fn test_library_empty_call_skipped() {
        // library() with no arguments should be skipped
        let code = "library()";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_non_library_call_ignored() {
        // Other function calls should be ignored
        let code = "print(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_with_other_arguments() {
        // library() with additional arguments
        let code = "library(dplyr, quietly = TRUE, warn.conflicts = FALSE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_mixed_library_and_source_calls() {
        // Mix of library() and source() calls
        let code = r#"library(dplyr)
source("utils.R")
library(ggplot2)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "ggplot2");
    }

    #[test]
    fn test_library_function_scope_is_none() {
        // Verify function_scope is None (will be populated later)
        let code = "library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert!(lib_calls[0].function_scope.is_none());
    }

    // ==================== for-loop library detection ====================

    #[test]
    fn static_for_library_loop_attaches_after_loop() {
        let code = r#"
packages <- c("alpha", "beta", NULL)
for (package in packages) {
  if (!requireNamespace(package, quietly = TRUE)) {
    install.packages(package)
  }
  library(package, character.only = TRUE)
}
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.package.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert!(calls.iter().all(|call| call.attaches));
        assert!(calls.iter().all(|call| call.line == 7));
    }

    #[test]
    fn static_inline_for_require_loop_is_detected() {
        let code =
            r#"for (package in c("alpha", NULL, "beta")) require(package, character.only = T)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.package.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn loop_checkpoint_does_not_weaken_post_loop_binding_barrier() {
        let code = r#"
packages <- c("alpha")
for (package in packages) library(package, character.only = TRUE)
sapply(packages, library, character.only = TRUE)
packages <- c("beta")
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(
            calls
                .iter()
                .map(|call| (call.package.as_str(), call.line))
                .collect::<Vec<_>>(),
            [("alpha", 2)]
        );
    }

    #[test]
    fn loop_checkpoint_honors_prior_persistent_unknown_mutation() {
        let code = r#"
packages <- c("alpha")
if (flag) assign(dynamic_name, c("beta"))
for (package in packages) library(package, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(
            calls.iter().all(|call| call.package != "alpha"),
            "{calls:?}"
        );
    }

    #[test]
    fn eager_capture_can_checkpoint_named_vector_but_deferred_function_cannot() {
        let eager = r#"
packages <- c("alpha")
bquote(.(for (package in packages) library(package, character.only = TRUE)))
"#;
        let eager_calls = detect_library_calls(&parse_r(eager), eager);
        assert_eq!(
            eager_calls
                .iter()
                .map(|call| call.package.as_str())
                .collect::<Vec<_>>(),
            ["alpha"]
        );

        let deferred = r#"
packages <- c("alpha")
f <- function() {
  for (package in packages) library(package, character.only = TRUE)
}
"#;
        let deferred_calls = detect_library_calls(&parse_r(deferred), deferred);
        assert!(
            deferred_calls.iter().all(|call| call.package != "alpha"),
            "{deferred_calls:?}"
        );
    }

    #[test]
    fn loop_loader_rejects_iterator_writes_and_shadowed_helpers() {
        for code in [
            "packages <- c(\"alpha\")\nfor (package in packages) { package <- \"other\"; library(package, character.only = TRUE) }",
            "packages <- c(\"alpha\")\nfor (package in packages) { assign(\"package\", \"other\"); library(package, character.only = TRUE) }",
            "packages <- c(\"alpha\")\nfor (package in packages) { rm(package); library(package, character.only = TRUE) }",
            "packages <- c(\"alpha\")\nlibrary <- function(...) NULL\nfor (package in packages) library(package, character.only = TRUE)",
            "packages <- c(\"alpha\")\nrequire <- function(...) NULL\nfor (package in packages) require(package, character.only = TRUE)",
        ] {
            let calls = detect_library_calls(&parse_r(code), code);
            assert!(
                calls.iter().all(|call| call.package != "alpha"),
                "{code}: {calls:?}"
            );
        }
    }

    #[test]
    fn dynamic_or_nonmandatory_for_loaders_are_skipped() {
        for code in [
            "for (package in packages) library(package, character.only = TRUE)",
            "packages <- c(\"alpha\")\npackages <- get_packages()\nfor (package in packages) library(package, character.only = TRUE)",
            "packages <- c(\"alpha\")\nassign(target, value)\nfor (package in packages) library(package, character.only = TRUE)",
            "packages <- c(\"alpha\")\nfor (package in packages) library(other, character.only = TRUE)",
            "packages <- c(\"alpha\")\nfor (package in packages) library(package)",
            "packages <- c(\"alpha\")\nfor (package in packages) if (enabled) library(package, character.only = TRUE)",
            "packages <- c(\"alpha\")\nfor (package in packages) { if (skip) next; library(package, character.only = TRUE) }",
            "packages <- c(\"alpha\")\nfor (package in packages) { if (stop) break; library(package, character.only = TRUE) }",
            "packages <- c(\"alpha\", \"beta\")\nfor (package in packages) { library(package, character.only = TRUE); if (stop) break }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); return() }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); `return`() }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); base::return() }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); base:::return() }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); \"return\"() }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); base::\"return\"() }",
            "f <- function() for (package in c(\"alpha\", \"beta\")) { library(package, character.only = TRUE); \"base\"::return() }",
        ] {
            let tree = parse_r(code);
            let calls = detect_library_calls(&tree, code);
            assert!(
                calls.iter().all(|call| call.package != "alpha"),
                "loop inference unexpectedly attached alpha for {code}: {calls:?}"
            );
        }
    }

    // ==================== apply-family library detection (#172) ====================

    #[test]
    fn test_apply_inline_c_with_library() {
        // sapply(c("dplyr","tidyr"), library, character.only = TRUE)
        // Validates issue #172 — the "inline c()" path.
        let code = r#"sapply(c("dplyr", "tidyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
        // Both share the apply call's end position.
        assert_eq!(lib_calls[0].line, 0);
        assert_eq!(lib_calls[1].line, 0);
        assert_eq!(lib_calls[0].column, lib_calls[1].column);
        assert!(lib_calls[0].function_scope.is_none());
    }

    #[test]
    fn test_apply_lapply_inline_c_with_require() {
        let code = r#"lapply(c("dplyr"), require, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_apply_vapply_inline_c() {
        // vapply has extra signature args; library FUN + c() X still detects.
        let code = r#"vapply(c("dplyr","tidyr"), require, logical(1), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_mapply_inline_c() {
        // mapply puts FUN first; we're position-agnostic so it still matches.
        let code = r#"mapply(library, c("dplyr","tidyr"), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_with_named_x_arg_skipped() {
        // A c() inside a *named* arg is not picked up — only positional X args
        // are considered. Documents the limitation.
        let code = r#"sapply(X = c("dplyr"), FUN = require, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_purrr_bare_walk() {
        let code = r#"walk(c("dplyr","tidyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_purrr_qualified_map() {
        let code = r#"purrr::map(c("dplyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_apply_purrr_qualified_map_chr() {
        let code = r#"purrr::map_chr(c("dplyr","tidyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
    }

    #[test]
    fn test_apply_other_namespace_not_detected() {
        // foo::map(...) is not purrr — skip.
        let code = r#"foo::map(c("dplyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_single_arrow_assignment() {
        // Same-file variable assigned exactly once via `<-` to a c() of strings.
        let code = "libs <- c(\"dplyr\", \"tidyr\")\nsapply(libs, require, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
        assert_eq!(lib_calls[0].line, 1);
        assert_eq!(lib_calls[1].line, 1);
    }

    #[test]
    fn package_vector_bindings_are_collected_only_on_demand() {
        let code = "packages <- c(\"alpha\")\nfor (x in packages) print(x)\nlibrary(dplyr)\nrequire(tidyr)";
        let tree = parse_r(code);
        let root = tree.root_node();
        let mut bindings = super::super::static_path::LazyStaticBindings::new(root, code);
        let mut output = LibraryWalkOutput::default();
        visit_node_for_library(
            root,
            code,
            &mut bindings,
            false,
            RuntimeFunctionScope::Lexical,
            true,
            &mut output,
        );
        assert!(!bindings.is_collected());

        let code = "libs <- c(\"dplyr\")\nsapply(libs, library, character.only = TRUE)\nlapply(libs, require, character.only = TRUE)";
        let tree = parse_r(code);
        let root = tree.root_node();
        let mut bindings = super::super::static_path::LazyStaticBindings::new(root, code);
        let mut output = LibraryWalkOutput::default();
        visit_node_for_library(
            root,
            code,
            &mut bindings,
            false,
            RuntimeFunctionScope::Lexical,
            true,
            &mut output,
        );
        assert!(bindings.is_collected());
        assert_eq!(output.library_calls.len(), 2);
    }

    #[test]
    fn artifact_detectors_share_one_static_binding_collection() {
        let code = r#"
path <- "child.R"
libs <- c("dplyr")
source(path)
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let root = tree.root_node();
        let mut bindings = super::super::static_path::LazyStaticBindings::new(root, code);

        let sources = detect_source_calls_with_bindings(&tree, code, &mut bindings);
        assert_eq!(sources.len(), 1, "{sources:?}");
        let collection = bindings.collection_address().unwrap();

        let libraries = detect_library_calls_with_bindings(&tree, code, &mut bindings);
        assert_eq!(libraries.len(), 1, "{libraries:?}");
        assert_eq!(libraries[0].package, "dplyr");
        assert_eq!(bindings.collection_address(), Some(collection));
    }

    #[test]
    fn later_deferred_helper_uncertainty_does_not_hide_tar_option_packages() {
        let code = r#"
library(targets)
tar_option_set(packages = c("shiny"))
server <- function(input, output, session) {
  output$plot <- renderPlot({ inner <- 1 })
}
"#;
        let packages = detect_targets_pipeline_packages(&parse_r(code), code);
        assert!(
            packages
                .iter()
                .any(|declaration| declaration.package == "shiny"),
            "got: {packages:?}"
        );
    }

    #[test]
    fn proven_non_mutating_constructs_preserve_package_candidates() {
        for intervening in [
            "for (i in NULL) libs <- c(\"tidyr\")",
            "rm(list = NULL)",
            "rm(list = base::character())",
            "rm(list = base::character(0))",
            "rm(list = base::character(length = 0))",
            "rm(list = character())",
            "rm(list = character(0))",
            "x <- base::paste0(\"a\", \"b\")",
            "x <- base::paste(\"a\", \"b\")",
            "quote(rm(libs))",
            "quote(libs <- c(\"tidyr\"))",
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{intervening}\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().any(|call| call.package == "dplyr"),
                "{intervening}: {calls:?}"
            );
        }
    }

    #[test]
    fn root_bquote_splice_keeps_where_package_before_operand_error() {
        let code = r#"bquote(..(library(operandpkg)), where = { library(wherepkg); parent.frame() }, splice = TRUE)"#;
        let calls = detect_library_calls(&parse_r(code), code);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].package, "wherepkg");
        assert_eq!(
            extract_attached_packages(code)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["wherepkg"]
        );
    }

    #[test]
    fn bquote_splice_result_gates_later_package_effects() {
        let code = r#"base::bquote(..(function() {}) + .(library(dplyr)), splice = TRUE)"#;
        assert!(detect_library_calls(&parse_r(code), code).is_empty());
        assert!(extract_attached_packages(code).is_empty());

        let code = r#"base::bquote(..(unknown) + .(library(dplyr)), splice = TRUE)"#;
        assert!(detect_library_calls(&parse_r(code), code).is_empty());
        assert!(extract_attached_packages(code).is_empty());

        for operand in ["1", r#""value""#, "list(1)", "c(1)", "base::list(1)"] {
            let code = format!("base::bquote(..({operand}) + .(library(dplyr)), splice = TRUE)");
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert_eq!(calls.len(), 1, "{code}: {calls:?}");
            assert_eq!(calls[0].package, "dplyr", "{code}");
            assert_eq!(
                extract_attached_packages(&code)
                    .into_iter()
                    .collect::<Vec<_>>(),
                vec!["dplyr"],
                "{code}"
            );
        }
    }

    #[test]
    fn bquote_where_removal_prevents_computed_package_effect() {
        let code = r#"
        libs <- c("tidyr")
        bquote(
            .(sapply(libs, library, character.only = TRUE)),
            where = { rm(libs); parent.frame() }
        )
        "#;
        let calls = detect_library_calls(&parse_r(code), code);
        assert!(calls.is_empty(), "{calls:?}");
    }

    #[test]
    fn package_detection_validates_rlang_capture_contracts() {
        for helper in ["expr", "quo", "enquo", "enexpr"] {
            let code = format!(r#"rlang::{helper}(!!library({helper}pkg), unused = 2)"#);
            assert!(
                detect_library_calls(&parse_r(&code), &code).is_empty(),
                "{helper}"
            );
            assert!(extract_attached_packages(&code).is_empty(), "{helper}");
        }

        let malformed = r#"rlang::quos(!!library(badpkg), .named = FALSE, .named = TRUE)"#;
        assert!(detect_library_calls(&parse_r(malformed), malformed).is_empty());
        assert!(extract_attached_packages(malformed).is_empty());
    }

    #[test]
    fn package_detection_respects_bquote_splice_control() {
        for (code, expected) in [
            (r#"bquote(..(library(defaultpkg)))"#, false),
            (r#"bquote(..(library(falsepkg)), splice = FALSE)"#, false),
            (r#"bquote(..(library(unknownpkg)), splice = flag)"#, false),
            (r#"bquote(..(library(rooterrorpkg)), splice = TRUE)"#, false),
            (
                r#"bquote(list(..(library(nestedpkg))), splice = TRUE)"#,
                true,
            ),
            (r#"bquote(.(library(dotpkg)), splice = FALSE)"#, true),
        ] {
            let calls = detect_library_calls(&parse_r(code), code);
            assert_eq!(!calls.is_empty(), expected, "{code}: {calls:?}");
            let attached = extract_attached_packages(code);
            assert_eq!(!attached.is_empty(), expected, "{code}: {attached:?}");
        }
    }

    #[test]
    fn package_detection_traverses_nested_dot_inside_disabled_dot_dot() {
        for (code, expected) in [
            (r#"bquote(..(.(library(omittedpkg))))"#, "omittedpkg"),
            (
                r#"bquote(list(..(list(.(library(nestedfalsepkg))))), splice = FALSE)"#,
                "nestedfalsepkg",
            ),
        ] {
            let calls = detect_library_calls(&parse_r(code), code);
            assert_eq!(calls.len(), 1, "{code}: {calls:?}");
            assert_eq!(calls[0].package, expected, "{code}");
        }
    }

    #[test]
    fn package_detection_ignores_extra_bquote_macro_actuals() {
        for (code, expected) in [
            (
                r#"bquote(.(library(dotfirstpkg), library(dotextrapkg)))"#,
                "dotfirstpkg",
            ),
            (
                r#"bquote(list(..(library(splicefirstpkg), library(spliceextrapkg))), splice = TRUE)"#,
                "splicefirstpkg",
            ),
            (
                r#"bquote(list(.(list(.(library(nestedfirstpkg))), library(nestedextrapkg))))"#,
                "nestedfirstpkg",
            ),
        ] {
            let calls = detect_library_calls(&parse_r(code), code);
            assert_eq!(calls.len(), 1, "{code}: {calls:?}");
            assert_eq!(calls[0].package, expected, "{code}");
        }
    }

    #[test]
    fn package_detection_uses_direct_splice_runtime_order_prefix() {
        let code = r#"bquote(list(.(library(headpkg)), ..(library(splicepkg))), splice = TRUE)"#;
        let calls = detect_library_calls(&parse_r(code), code);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].package, "splicepkg");
    }

    #[test]
    fn unknown_bquote_splice_suppresses_later_package_effects() {
        let code = r#"base::bquote(list(..(unknown), .(library(tailpkg))), splice = { library(controlpkg); flag })"#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["controlpkg"]);
        assert_eq!(
            extract_attached_packages(code)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["controlpkg"]
        );
    }

    #[test]
    fn named_unknown_bquote_splice_before_expr_suppresses_package_effects() {
        let code = r#"base::bquote(splice = flag, expr = list(.(library(prefixpkg)), ..(library(operandpkg)), .(library(tailpkg))))"#;
        assert!(detect_library_calls(&parse_r(code), code).is_empty());
        assert!(extract_attached_packages(code).is_empty());

        let code = r#"base::bquote(splice = { library(controlpkg); flag }, expr = list(.(library(prefixpkg)), ..(library(operandpkg)), .(library(tailpkg))))"#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["controlpkg"]);
        assert_eq!(
            extract_attached_packages(code)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["controlpkg"]
        );
    }

    #[test]
    fn package_detection_emits_nothing_when_capture_formal_matching_fails() {
        for code in [
            r#"substitute(expr = library(substitutepkg), expr = x)"#,
            r#"bquote(expr = library(bquotepkg), expr = x)"#,
        ] {
            let calls = detect_library_calls(&parse_r(code), code);
            assert!(calls.is_empty(), "{code}: {calls:?}");
            let attached = extract_attached_packages(code);
            assert!(attached.is_empty(), "{code}: {attached:?}");
        }
    }

    #[test]
    fn bquote_unknown_splice_conservatively_invalidates_package_vectors() {
        for (capture, preserves) in [
            (
                r#"bquote(..(base::assign("libs", c("tidyr"), envir = .GlobalEnv)))"#,
                true,
            ),
            (
                r#"bquote(..(base::assign("libs", c("tidyr"), envir = .GlobalEnv)), splice = FALSE)"#,
                true,
            ),
            (
                r#"bquote(..(base::assign("libs", c("tidyr"), envir = .GlobalEnv)), splice = flag)"#,
                true,
            ),
            (
                r#"bquote(list(..(base::assign("libs", c("tidyr"), envir = .GlobalEnv))), splice = flag)"#,
                false,
            ),
            (
                r#"bquote(..(base::assign("libs", c("tidyr"), envir = .GlobalEnv)), splice = TRUE)"#,
                true,
            ),
            (
                r#"bquote(list(..(base::assign("libs", c("tidyr"), envir = .GlobalEnv))), splice = TRUE)"#,
                false,
            ),
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{capture}\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert_eq!(
                calls.iter().any(|call| call.package == "dplyr"),
                preserves,
                "{capture}: {calls:?}"
            );
        }
    }

    #[test]
    fn unknown_bquote_splice_false_branch_invalidates_package_vector() {
        for (capture, preserves) in [
            (
                r#"base::bquote(list(..(1, .(base::assign("libs", c("tidyr"))))), splice = flag)"#,
                false,
            ),
            (
                r#"base::bquote(list(..(1, .(base::assign("libs", c("tidyr"))))), splice = TRUE)"#,
                true,
            ),
            (
                r#"base::bquote(list(..(base::quote(.(base::assign("libs", c("tidyr")))))), splice = flag)"#,
                false,
            ),
            (
                r#"base::bquote(list(..(base::quote(.(base::assign("libs", c("tidyr")))))), splice = TRUE)"#,
                true,
            ),
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{capture}\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert_eq!(
                calls.iter().any(|call| call.package == "dplyr"),
                preserves,
                "{capture}: {calls:?}"
            );
        }
    }

    #[test]
    fn evaluated_capture_mutations_invalidate_package_vectors() {
        for evaluated in [
            r#"bquote(.(base::assign("libs", c("tidyr"), envir = .GlobalEnv)))"#,
            r#"substitute(x, env = base::assign("libs", c("tidyr"), envir = .GlobalEnv))"#,
            r#"quote <- function(x) force(x)
quote(base::assign("libs", c("tidyr"), envir = .GlobalEnv))"#,
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{evaluated}\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{evaluated}: {calls:?}"
            );
        }
    }

    #[test]
    fn transparent_wrapped_assignments_supply_package_candidates() {
        for assignment in ["{ libs <- c(\"dplyr\") }", "(libs <- c(\"dplyr\"))"] {
            let code = format!("{assignment}\nsapply(libs, library, character.only = TRUE)");
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().any(|call| call.package == "dplyr"),
                "{assignment}: {calls:?}"
            );
        }
    }

    #[test]
    fn immediate_bquote_bindings_resolve_package_vectors() {
        for binding in [
            r#"bquote(.(libs <- c("dplyr", "tidyr")))"#,
            r#"bquote(.(assign("libs", c("dplyr", "tidyr"))))"#,
        ] {
            let code = format!("{binding}\nsapply(libs, library, character.only = TRUE)");
            let packages: Vec<_> = detect_library_calls(&parse_r(&code), &code)
                .into_iter()
                .map(|call| call.package)
                .collect();
            assert_eq!(packages, ["dplyr", "tidyr"], "{binding}");
        }
    }

    #[test]
    fn immediate_bquote_package_bindings_preserve_conservative_gates() {
        for setup in [
            r#"assign <- function(...) NULL
bquote(.(assign("libs", c("dplyr"))))"#,
            r#"c <- function(...) "dplyr"
bquote(.(libs <- c("dplyr")))"#,
            r#"c <- function(...) "dplyr"
bquote(.(assign("libs", c("dplyr"))))"#,
            r#"bquote(.(libs <- c("dplyr")), where = new.env())"#,
            r#"bquote(.(assign("libs", c("dplyr"))), where = new.env())"#,
            r#"bquote(.(assign("libs", c("dplyr"), envir = new.env())))"#,
            r#"f <- function() bquote(.(libs <- c("dplyr")))"#,
            r#"f <- function() bquote(.(assign("libs", c("dplyr"))))"#,
        ] {
            let code = format!("{setup}\nsapply(libs, library, character.only = TRUE)");
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{setup}: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_equals_assignment() {
        let code = "libs = c(\"dplyr\", \"tidyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_assign_call() {
        let code = "assign(\"libs\", c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_base_qualified_assign_call() {
        for assign in [
            "base::assign",
            "base:::assign",
            "\"base\"::assign",
            "base::\"assign\"",
            "base::`assign`",
        ] {
            let code = format!(
                "{assign}(\"libs\", c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let lib_calls = detect_library_calls(&tree, &code);
            assert_eq!(lib_calls.len(), 2, "{assign}");
            assert_eq!(lib_calls[0].package, "dplyr", "{assign}");
            assert_eq!(lib_calls[1].package, "tidyr", "{assign}");
        }
    }

    #[test]
    fn test_apply_var_assign_inherits_false_uses_default_destination() {
        for assign in ["assign", "base::assign", "base:::assign"] {
            let code = format!(
                "{assign}(\"libs\", c(\"dplyr\", \"tidyr\"), inherits = FALSE)\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert_eq!(calls.len(), 2, "{assign}: {calls:?}");
            assert_eq!(calls[0].package, "dplyr");
            assert_eq!(calls[1].package, "tidyr");
        }
    }

    #[test]
    fn test_apply_and_tar_assign_pos_one_is_global() {
        for assign in ["assign", "base::assign", "base:::assign"] {
            let code = format!(
                "{assign}(\"libs\", c(\"dplyr\"), pos = 1, inherits = FALSE)\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().any(|call| call.package == "dplyr"),
                "{assign}: {calls:?}"
            );

            let code = format!(
                "library(targets)\n{assign}(\"libs\", c(\"shiny\"), pos = 1)\ntar_option_set(packages = libs)"
            );
            let packages = detect_targets_pipeline_packages(&parse_r(&code), &code);
            assert!(
                packages
                    .iter()
                    .any(|declaration| declaration.package == "shiny"),
                "{assign}: {packages:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_assign_dynamic_inherits_is_not_a_candidate() {
        for inherits in ["F", "flag"] {
            let code = format!(
                "assign(\"libs\", c(\"dplyr\"), inherits = {inherits})\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{inherits}: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_qualified_or_non_global_assign_is_not_a_candidate() {
        for assignment in [
            "other::assign(\"libs\", c(\"dplyr\"))",
            "base::assign(\"libs\", c(\"dplyr\"), envir = new.env())",
            "base::assign(\"libs\", c(\"dplyr\"), pos = 2)",
            "base::assign(\"libs\", c(\"dplyr\"), pos = flag)",
            "base::assign(\"libs\", c(\"dplyr\"), pos = 1, envir = .GlobalEnv)",
            "base::assign(\"libs\", c(\"dplyr\"), pos = 1, inherits = TRUE)",
            "f <- function() base::assign(\"libs\", c(\"dplyr\"))",
            "f <- function() libs <- c(\"dplyr\")",
        ] {
            let code = format!("{assignment}\nsapply(libs, library, character.only = TRUE)");
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "assignment `{assignment}` supplied a package candidate: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_assignment_after_apply_call_skipped() {
        // Variable assigned *after* the apply call must not resolve.
        let code = "sapply(libs, library, character.only = TRUE)\nlibs <- c(\"dplyr\")";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_multiple_assignments_skipped() {
        let code = "libs <- c(\"dplyr\")\nlibs <- c(\"tidyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_super_assignment_disqualifies() {
        // <<- alone counts but doesn't extract — single-assignment but no
        // static packages means the binding doesn't resolve.
        let code = "libs <<- c(\"dplyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_function_param_shadow_disqualifies() {
        // A function parameter named `libs` increments the count and
        // disqualifies the global binding.
        let code = "libs <- c(\"dplyr\")\nf <- function(libs) {}\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_shared_binding_invalidators_disqualify() {
        // These forms were already load-bearing invalidators for static path
        // folding. Package-vector detection must count the same bindings so
        // the two consumers cannot drift back to stale single-assignment data.
        for mutation in [
            "libs[1] <- \"tidyr\"",
            "names(libs) <- \"package\"",
            "rm(libs)",
            "rm(list = c(\"libs\", \"other\"))",
            "remove(list = c(\"other\", \"libs\"))",
            "rm(list = base::c(\"libs\", \"other\"))",
            "rm(list = `c`(\"other\", \"libs\"))",
            "base::rm(libs)",
            "base:::remove(\"libs\")",
            "base::assign(\"libs\", c(\"tidyr\"))",
            "other::assign(\"libs\", c(\"tidyr\"))",
            "other::rm(libs)",
            "for (libs in list(c(\"tidyr\"))) {}",
            "libs %<>% identity()",
            "\"libs\" <- c(\"tidyr\")",
            "`libs` <- c(\"tidyr\")",
            "assign(\"x\" = \"libs\", \"value\" = c(\"tidyr\"))",
            r#"assign(`\x78` = "libs", value = c("tidyr"))"#,
            "rm(\"list\" = c(\"libs\"))",
            r#"rm(`l\x69st` = c("libs"))"#,
            "libs <- # changed\n  c(\"tidyr\")",
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{mutation}\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "mutation `{mutation}` left a stale package candidate: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_load_invalidation_is_destination_and_scope_aware() {
        for loader in [
            r#"load("state.RData")"#,
            r#"base::load("state.RData")"#,
            r#"sys.load.image("state.RData", quiet = TRUE)"#,
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{loader}\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{loader}: {calls:?}"
            );
        }

        for loader in [
            r#"base::load("state.RData", envir = base::new.env())"#,
            r#"f <- function() base::load("state.RData")"#,
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{loader}\nsapply(libs, library, character.only = TRUE)"
            );
            let calls = detect_library_calls(&parse_r(&code), &code);
            assert!(
                calls.iter().any(|call| call.package == "dplyr"),
                "{loader}: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_backtick_reference_uses_canonical_binding_key() {
        let code = "libs <- c(\"dplyr\")\nsapply(`libs`, library, character.only = TRUE)";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().any(|call| call.package == "dplyr"));

        let code = "sapply(c(\"dplyr\"), library, character.only = TRUE)\nrm(c)";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().any(|call| call.package == "dplyr"));
    }

    #[test]
    fn test_apply_var_malformed_remove_vector_does_not_invalidate() {
        for vector in [
            "c()",
            "c(\"libs\",)",
            "c(,\"libs\")",
            "c(\"other\",,\"libs\")",
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\nrm(list = {vector})\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().any(|call| call.package == "dplyr"),
                "malformed vector `{vector}` invalidated the package candidate: {calls:?}"
            );
        }

        for remove in [
            r#"rm(list = c("libs"), list = c("other"))"#,
            "rm(libs, pos = 1, pos = 2)",
            r#"rm(libs, pos = 1, pos = 2, `\x6cist` = c("libs"))"#,
            r#"assign(x = "libs", x = "other", `\x76alue` = c("tidyr"))"#,
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{remove}\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().any(|call| call.package == "dplyr"),
                "{remove}: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_var_dynamic_remove_list_invalidates_prior_candidates() {
        for (remove, setup, list) in [
            ("rm", "libs <- c(\"dplyr\")\ndplyr <- 1", "libs"),
            ("remove", "libs <- c(\"libs\")", "libs"),
            ("rm", "victims <- \"libs\"\nlibs <- c(\"dplyr\")", "victims"),
        ] {
            let code = format!(
                "{setup}\n{remove}(list = {list})\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{remove}(list = {list}) retained a stale package candidate: {calls:?}"
            );
        }

        let code = "victims <- \"libs\"\nrm(list = victims)\nlibs <- c(\"dplyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        // Evaluating an identifier-valued list may force an active or delayed
        // binding with arbitrary side effects, so helper trust stays disabled
        // even for a syntactically later vector assignment.
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        // A known base removal cannot shadow `c`, so a later package-vector
        // assignment remains usable. An unknown escaped binding target, by
        // contrast, may itself denote `c` and must disable helper-dependent
        // candidates even though it does not invalidate later literal paths.
        let code = r#"rm("\x6cibs")
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().any(|call| call.package == "dplyr"));

        let code = r#"`\x6cibs` <- c("old")
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"f <- function() {
  sapply(c("dplyr"), library, character.only = TRUE)
}
x <- {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  1
}
f()"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
name <- "c"
assign(name, function(...) "libs")
libs <- c("dplyr")
rm(list = c("other"))
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
name <- "c"
assign(name, function(...) "tidyr")
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        for list in [
            "c <- function(...) \"libs\"\nrm(list = c(\"other\"))",
            "`c` <- function(...) \"libs\"\nrm(list = c(\"other\"))",
            "rm(list = other::c(\"other\"))",
        ] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{list}\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{list} retained a stale package candidate: {calls:?}"
            );
        }
    }

    #[test]
    fn test_apply_dynamic_x_paste0_skipped() {
        let code = r#"sapply(paste0("dp", "lyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_dynamic_x_setdiff_skipped() {
        let code = r#"sapply(setdiff(c("a","b"), "b"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_dynamic_x_c_with_var_skipped() {
        // c() containing a non-string argument disqualifies the X arg.
        let code = "libs1 <- c(\"a\")\nsapply(c(libs1, \"b\"), library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_anonymous_fun_skipped() {
        // \(x) library(x) — FUN is not a bare identifier so the apply must not
        // pick up "dplyr". The inner `library(x)` may still be detected with
        // package="x" by the existing direct-library detector — that's
        // pre-existing loose behavior; we only assert the apply path didn't
        // fire by checking that no LibraryCall mentions "dplyr".
        let code = r#"sapply(c("dplyr"), \(x) library(x), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert!(
            lib_calls.iter().all(|c| c.package != "dplyr"),
            "apply path should not emit dplyr; got {:?}",
            lib_calls
        );
    }

    #[test]
    fn test_apply_no_character_only_skipped() {
        let code = r#"sapply(c("dplyr"), library)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_character_only_false_skipped() {
        let code = r#"sapply(c("dplyr"), library, character.only = FALSE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_loadnamespace_fun_skipped() {
        // loadNamespace is intentionally not in the FUN allowlist.
        let code = r#"sapply(c("dplyr"), loadNamespace, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_position_at_call_end_utf16() {
        // 🎉 is 4 UTF-8 bytes / 2 UTF-16 code units.
        let code = "🎉; sapply(c(\"dplyr\",\"tidyr\"), library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        let total_utf16 = code.encode_utf16().count() as u32;
        for call in &lib_calls {
            assert_eq!(call.line, 0);
            assert_eq!(call.column, total_utf16);
        }
    }

    #[test]
    fn test_apply_issue_172_exact_example() {
        // Issue #172 — exact pattern from the report.
        let code =
            "libs <- c(\"lib1\", \"lib2\", \"lib3\")\nsapply(libs, require, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 3);
        assert_eq!(lib_calls[0].package, "lib1");
        assert_eq!(lib_calls[1].package, "lib2");
        assert_eq!(lib_calls[2].package, "lib3");
        for call in &lib_calls {
            assert_eq!(call.line, 1);
        }
    }

    #[test]
    fn test_apply_issue_172_via_extract_metadata() {
        let code =
            "libs <- c(\"lib1\", \"lib2\", \"lib3\")\nsapply(libs, require, character.only = TRUE)";
        let meta = crate::cross_file::extract_metadata(code);
        let pkgs: Vec<&str> = meta
            .library_calls
            .iter()
            .map(|c| c.package.as_str())
            .collect();
        assert_eq!(pkgs, vec!["lib1", "lib2", "lib3"]);
    }

    #[test]
    fn test_apply_library_in_extra_position_skipped() {
        // FUN is `identity`; `library` is just an extra positional arg passed
        // through `...` to identity, which doesn't load anything.
        let code = r#"sapply(c("dplyr"), identity, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_library_at_x_position_skipped() {
        // For sapply/lapply/etc., X is at position 0 and FUN at position 1.
        // Swapping them is not a real library load.
        let code = r#"sapply(library, c("dplyr"), character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_call_named_args() {
        // assign(x = "libs", value = c(...)) — the named-arg form should
        // count and resolve just like the positional form.
        let code = "assign(x = \"libs\", value = c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_dynamic_assign_name_invalidates_prior_candidate() {
        for (setup, target) in [
            ("n <- \"libs\"\nlibs <- c(\"dplyr\")", "n"),
            ("libs <- c(\"dplyr\")", r#""\x6cibs""#),
        ] {
            let code = format!(
                "{setup}\nassign({target}, c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(
                calls.iter().all(|call| call.package != "dplyr"),
                "{target}: {calls:?}"
            );
        }

        let code = r#"
f <- function() assign("\x6cibs", c("tidyr"), envir = .GlobalEnv)
libs <- c("dplyr")
f()
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
f <- function() rm(list = c("other"), envir = .GlobalEnv)
libs <- c("dplyr")
c <- function(...) "libs"
f()
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
f <- function() rm(list = c("other",), envir = .GlobalEnv)
libs <- c("dplyr")
c <- function(...) "libs"
f()
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
later <- function(x) function() x
g <- later(rm(list = victims, envir = .GlobalEnv))
victims <- "libs"
libs <- c("dplyr")
g()
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
`%delay%` <- function(x, y) function() x
n <- "libs"
trigger <- assign(n, c("tidyr"), envir = .GlobalEnv) %delay% NULL
libs <- c("dplyr")
trigger()
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"
f <- function() assign("ignored", "x")
assign <- function(...) base::assign("libs", c("tidyr"), envir = .GlobalEnv)
libs <- c("dplyr")
f()
sapply(libs, library, character.only = TRUE)
"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"base::rm(`l\x69st` = {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  character()
})
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));
    }

    #[test]
    fn test_apply_var_shadowed_c_is_not_a_static_package_vector() {
        let code = r#"c <- function(...) "tidyr"
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"sapply(c("dplyr"), library, character.only = TRUE)
c <- function(...) "tidyr""#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().any(|call| call.package == "dplyr"));

        let code = r#"f <- function() {
  sapply(c("dplyr"), library, character.only = TRUE)
}
g <- function() {
  assign("\x63", function(...) "tidyr", envir = .GlobalEnv)
}
g()
f()"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"for (i in {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  list()
}) {}
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"x <- 0
x[{
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  1
}] <- 1
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code =
            "x <- 1\ny <- x\nlibs <- c(\"dplyr\")\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().any(|call| call.package == "dplyr"));

        for vector in [
            "c(\"dplyr\",)",
            "c(,\"dplyr\")",
            "c(\"dplyr\",,\"tidyr\")",
            r#"c("dpl\x79r")"#,
        ] {
            let code = format!("libs <- {vector}\nsapply(libs, library, character.only = TRUE)");
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(calls.iter().all(|call| call.package != "dplyr"));
        }

        let code = r#"c <- function(...) "tidyr"
assign("libs", c("dplyr"))
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));
    }

    #[test]
    fn test_deferred_unknown_mutation_disables_inline_c() {
        let code = r#"name <- "c"
f <- function() assign(name, function(...) "tidyr", envir = .GlobalEnv)
f()
sapply(c("dplyr"), library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));

        let code = r#"try(base::assign({
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  "unused"
}), silent = TRUE)
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));
    }

    #[test]
    fn test_dynamic_rm_arguments_can_shadow_inline_c() {
        for (setup, argument) in [
            (
                "",
                r#"list = {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  character()
}"#,
            ),
            (
                "",
                r#"envir = {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  .GlobalEnv
}"#,
            ),
            (
                r#"delayedAssign("e", {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  .GlobalEnv
})
"#,
                "list = base::c(), envir = e",
            ),
            (
                r#"`-` <- function(x) {
  get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
  .GlobalEnv
}
"#,
                "list = base::c(), pos = -1",
            ),
        ] {
            let code = format!(
                "{setup}rm({argument})\nsapply(c(\"dplyr\"), library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(calls.iter().all(|call| call.package != "dplyr"));
        }

        let code = r#"try(
  base::rm(list = base::c({
    get("assign", baseenv())("c", function(...) "tidyr", envir = .GlobalEnv)
    "other"
  },)),
  silent = TRUE
)
libs <- c("dplyr")
sapply(libs, library, character.only = TRUE)"#;
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.iter().all(|call| call.package != "dplyr"));
    }

    #[test]
    fn test_apply_var_variadic_parameter_does_not_invalidate_candidate() {
        for parameter in ["...", "..1"] {
            let code = format!(
                "libs <- c(\"dplyr\")\nf <- function({parameter}) NULL\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(calls.iter().any(|call| call.package == "dplyr"));
        }
    }

    #[test]
    fn test_apply_var_assign_named_overrides_disqualifies() {
        // libs is assigned twice — once via `<-`, once via named-arg assign().
        // The named assign() must count toward the multi-assignment rule.
        let code = "libs <- c(\"dplyr\")\nassign(x = \"libs\", value = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_map_if_library_in_predicate_slot_not_detected() {
        // map_if has signature (.x, .p, .f) — the predicate is at position 1,
        // not the FUN. Putting `library` in the predicate slot doesn't load
        // anything; we should not match it. Drop map_if/map_at from the
        // supported apply set rather than guessing positions per signature.
        let code = r#"map_if(c("dplyr"), library, is.character, character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_partial_named() {
        // R does partial matching after exact: `val` is a unique prefix of
        // `value` among assign()'s formals, so `val = ...` binds `value`.
        let code = "assign(x = \"libs\", val = c(\"dplyr\", \"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "tidyr");
    }

    #[test]
    fn test_apply_var_assign_partial_named_overrides_disqualifies() {
        // Same as test_apply_var_assign_named_overrides_disqualifies but the
        // second assign uses partial-match `val` for `value`.
        let code = "libs <- c(\"dplyr\")\nassign(x = \"libs\", val = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_exact_and_partial_value_skipped() {
        // R does exact-name matching first, then partial. With both an exact
        // `value = ...` and a partial `val = ...` present, R errors with
        // "matched by multiple actual arguments" and the assignment never
        // happens; we must not record a static binding from it.
        let code = "assign(x = \"libs\", val = c(\"dplyr\"), value = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_duplicate_exact_value_skipped() {
        // Same idea: two exact `value =` args also error in R.
        let code = "assign(x = \"libs\", value = c(\"dplyr\"), value = c(\"tidyr\"))\nsapply(libs, library, character.only = TRUE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_apply_var_assign_rejects_errors_in_other_formals() {
        // assign() has no `...`: an unknown name, an ambiguous partial name,
        // or a duplicate non-x/value formal errors before the binding occurs.
        for args in [
            r#"x = "libs", value = c("dplyr"), bogus = 1"#,
            r#"x = "libs", value = c("dplyr"), i = TRUE"#,
            r#"x = "libs", value = c("dplyr"), pos = 1, pos = 2"#,
            r#""libs", c("dplyr"), , , , , 1"#,
        ] {
            let code = format!("assign({args})\nsapply(libs, library, character.only = TRUE)");
            let tree = parse_r(&code);
            let lib_calls = detect_library_calls(&tree, &code);
            assert!(
                lib_calls.is_empty(),
                "unexpected calls for {args}: {lib_calls:?}"
            );
        }

        for assign in [r#"assign("libs")"#, r#"assign("libs", value = )"#] {
            let code = format!(
                "libs <- c(\"dplyr\")\n{assign}\nsapply(libs, library, character.only = TRUE)"
            );
            let tree = parse_r(&code);
            let calls = detect_library_calls(&tree, &code);
            assert!(calls.iter().any(|call| call.package == "dplyr"));
        }
    }

    // ==================== tar_option_set package detection (#637) ====================

    /// Convenience: the distinct file/pipeline-level targets package channel.
    fn tar_packages(code: &str) -> Vec<TargetsPackageDeclaration> {
        detect_targets_pipeline_packages(&parse_r(code), code)
    }

    #[test]
    fn test_tar_option_set_inline_c_with_library_targets() {
        let code = "library(targets)\ntar_option_set(packages = c(\"dplyr\", \"tidyr\"))";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[1].package, "tidyr");
        // Each package is anchored at its OWN string literal, not the call end.
        assert_eq!(calls[0].line, 1);
        assert_eq!(calls[1].line, 1);
        assert_ne!(
            calls[0].column, calls[1].column,
            "per-literal anchoring: distinct literals have distinct columns"
        );
        assert!(calls[0].column < calls[1].column);
    }

    #[test]
    fn test_tar_option_set_single_string_literal() {
        let code = "library(targets)\ntar_option_set(packages = \"dplyr\")";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[0].line, 1);
    }

    #[test]
    fn test_tar_option_set_qualified_without_library_targets() {
        // Qualified spellings are accepted unconditionally — no library(targets)
        // needed.
        let code = "targets::tar_option_set(packages = c(\"dplyr\"))";
        let packages = tar_packages(code);
        assert_eq!(packages.len(), 1, "got: {packages:?}");
        assert_eq!(packages[0].package, "dplyr");

        let code = "targets:::tar_option_set(packages = c(\"dplyr\"))";
        let packages = tar_packages(code);
        assert_eq!(packages.len(), 1, "got: {packages:?}");
        assert_eq!(packages[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_bare_shadowed_but_qualified_still_detected() {
        let code = "library(targets)\ntar_option_set <- function(...) NULL\ntar_option_set(packages = \"dplyr\")";
        assert!(tar_packages(code).is_empty());

        let code =
            "tar_option_set <- function(...) NULL\ntargets::tar_option_set(packages = \"dplyr\")";
        let packages = tar_packages(code);
        assert_eq!(packages.len(), 1, "got: {packages:?}");
        assert_eq!(packages[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_bare_without_library_targets_skipped() {
        // The bare spelling requires targets to be attached somewhere in the
        // file (targets-in-play gate).
        let code = "tar_option_set(packages = c(\"dplyr\"))";
        let packages = tar_packages(code);
        assert!(packages.is_empty(), "got: {packages:?}");
    }

    #[test]
    fn test_tar_option_set_bare_gate_is_position_independent() {
        // library(targets) AFTER the call still satisfies the gate.
        let code = "tar_option_set(packages = \"dplyr\")\nlibrary(targets)";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_bare_loadnamespace_targets_does_not_satisfy_gate() {
        // loadNamespace("targets") does not attach, so it does not enable the
        // bare spelling.
        let code = "loadNamespace(\"targets\")\ntar_option_set(packages = c(\"dplyr\"))";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_var_resolved_anchored_at_call_end() {
        let code =
            "library(targets)\npkgs <- c(\"dplyr\", \"tidyr\")\ntar_option_set(packages = pkgs)";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[1].package, "tidyr");
        // No literal at the call site — both share the call's end position.
        assert_eq!(calls[0].line, 2);
        assert_eq!(calls[1].line, 2);
        assert_eq!(calls[0].column, calls[1].column);
        let call_len = "tar_option_set(packages = pkgs)".len() as u32;
        assert_eq!(calls[0].column, call_len);
    }

    #[test]
    fn test_tar_option_set_var_replacement_binding_disqualifies() {
        let code = "library(targets)\npkgs <- c(\"dplyr\")\npkgs[1] <- \"tidyr\"\ntar_option_set(packages = pkgs)";
        assert!(tar_packages(code).is_empty());
    }

    #[test]
    fn test_tar_option_set_positional_packages_skipped() {
        // tar_option_set's first formal is tidy_eval, so positional matching
        // is not attempted (documented limitation).
        let code = "library(targets)\ntar_option_set(TRUE, c(\"dplyr\"))";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_dynamic_value_skipped() {
        let code = "library(targets)\ntar_option_set(packages = getOption(\"my.pkgs\"))";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_no_packages_arg_skipped() {
        let code = "library(targets)\ntar_option_set(format = \"qs\")";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");
    }

    #[test]
    fn test_tar_option_set_character0_and_empty_c_skipped() {
        let code = "library(targets)\ntar_option_set(packages = character(0))";
        assert_eq!(tar_packages(code).len(), 0);
        let code = "library(targets)\ntar_option_set(packages = c())";
        assert_eq!(tar_packages(code).len(), 0);
    }

    #[test]
    fn test_tar_option_set_unrelated_named_args_still_detected() {
        let code = "library(targets)\ntar_option_set(packages = c(\"dplyr\"), format = \"qs\", memory = \"transient\")";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
    }

    #[test]
    fn test_tar_option_set_multiple_calls_union() {
        // targets' runtime is last-call-wins, but raven deliberately unions
        // (favoring false negatives) — see try_parse_tar_option_set_call.
        let code = "library(targets)\ntar_option_set(packages = c(\"dplyr\"))\ntar_option_set(packages = c(\"tidyr\"))";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[1].package, "tidyr");
    }

    #[test]
    fn test_tar_option_set_multiline_anchors_at_literal_line() {
        // The diagnostic range is built from LibraryCall.line, and
        // `# raven: ignore` suppression is line-keyed, so each package must
        // anchor at its own literal's line — not the closing paren's.
        let code = "library(targets)\ntar_option_set(\n  packages = c(\n    \"dplyr\",\n    \"tidyr\"\n  ),\n  format = \"qs\"\n)";
        let calls = tar_packages(code);
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        assert_eq!(calls[0].package, "dplyr");
        assert_eq!(calls[0].line, 3, "anchored at its own literal's line");
        assert_eq!(calls[1].package, "tidyr");
        assert_eq!(calls[1].line, 4, "anchored at its own literal's line");
    }

    #[test]
    fn extract_attached_packages_excludes_top_level_tar_option_set() {
        let pkgs =
            extract_attached_packages("library(targets)\ntar_option_set(packages = c(\"dplyr\"))");
        assert!(pkgs.contains("targets"));
        assert!(!pkgs.contains("dplyr"));

        // Pipeline worker packages must not become test preamble attachments.
        let pkgs = extract_attached_packages("targets::tar_option_set(packages = \"dplyr\")");
        assert!(!pkgs.contains("dplyr"));
    }

    #[test]
    fn extract_attached_packages_excludes_function_body_tar_option_set() {
        let pkgs = extract_attached_packages(
            "library(targets)\nf <- function() tar_option_set(packages = c(\"dplyr\"))",
        );
        assert!(!pkgs.contains("dplyr"), "got: {pkgs:?}");
    }

    #[test]
    fn extract_attached_packages_excludes_quote_wrapped_tar_option_set() {
        let pkgs = extract_attached_packages(
            "library(targets)\nq <- quote(tar_option_set(packages = c(\"dplyr\")))",
        );
        assert!(!pkgs.contains("dplyr"), "got: {pkgs:?}");
    }

    #[test]
    fn test_tar_option_set_quote_wrapped_skipped_on_position_aware_path() {
        // A quoted tar_option_set() captures code without evaluating it, so
        // detect_library_calls must not record attachments — even for the
        // unconditionally-honored qualified spelling.
        let code = "q <- quote(targets::tar_option_set(packages = \"dplyr\"))";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(calls.len(), 0, "got: {calls:?}");

        // Bare spelling under quote() with targets attached: only the
        // library(targets) itself is recorded.
        let code = "library(targets)\nq <- quote(tar_option_set(packages = \"dplyr\"))";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].package, "targets");

        // Direct package loads obey the same proven-capture boundary.
        let code = "q <- quote(library(dplyr))";
        let tree = parse_r(code);
        let calls = detect_library_calls(&tree, code);
        assert!(calls.is_empty(), "got: {calls:?}");
    }

    #[test]
    fn targets_packages_are_separate_and_source_sorted() {
        let code = "tar_option_set(packages = c(\"aaa\", \"bbb\"))\nlibrary(targets)\nlibrary(zzz)";
        let tree = parse_r(code);

        let calls = detect_library_calls(&tree, code);
        let loaded: Vec<&str> = calls.iter().map(|call| call.package.as_str()).collect();
        assert_eq!(loaded, vec!["targets", "zzz"]);

        let declarations = detect_targets_pipeline_packages(&tree, code);
        let positions: Vec<(u32, u32)> = declarations
            .iter()
            .map(|declaration| (declaration.line, declaration.column))
            .collect();
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(positions, sorted, "got: {declarations:?}");
        let packages: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.package.as_str())
            .collect();
        assert_eq!(packages, vec!["aaa", "bbb"]);
    }

    // ==================== system.file() detection in source() ====================

    #[test]
    fn test_source_system_file_single_part() {
        let code = r#"source(system.file("helper.R", package = "Matrix"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].path.is_empty());
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["helper.R"]);
        assert_eq!(sf.package, "Matrix");
    }

    #[test]
    fn test_source_system_file_multi_part() {
        let code = r#"source(system.file("a", "b.R", package = "P"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["a", "b.R"]);
        assert_eq!(sf.package, "P");
    }

    #[test]
    fn test_source_system_file_non_literal_arg_skipped() {
        // A variable positional arg → bail (unresolved = no ForwardSource)
        let code = r#"source(system.file(x, package = "P"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_no_package_skipped() {
        // No package= arg → not parseable
        let code = r#"source(system.file("helper.R"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_variable_package_skipped() {
        // Non-literal package= → bail
        let code = r#"source(system.file("helper.R", package = pkg))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_lib_loc_rejected() {
        // lib.loc alters search path — unresolvable statically
        let code = r#"source(system.file("x.R", package = "p", lib.loc = foo))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_fsep_rejected() {
        // Non-default fsep alters path construction — unresolvable statically
        let code = r#"source(system.file("x.R", package = "p", fsep = "\\"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_system_file_must_work_accepted() {
        // mustWork doesn't affect path layout — still resolvable
        let code = r#"source(system.file("x.R", package = "p", mustWork = FALSE))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }

    #[test]
    fn test_source_system_file_lib_loc_dot_library_accepted() {
        // .Library is the default library path — safe to resolve as-if-absent
        let code = r#"source(system.file("test-tools-1.R", package = "Matrix", lib.loc = .Library), keep.source = FALSE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["test-tools-1.R"]);
        assert_eq!(sf.package, "Matrix");
    }

    #[test]
    fn test_source_system_file_lib_loc_lib_paths_accepted() {
        // .libPaths() returns the default search paths — safe to resolve
        let code = r#"source(system.file("x.R", package = "p", lib.loc = .libPaths()))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }

    #[test]
    fn test_source_system_file_lib_loc_null_accepted() {
        // lib.loc = NULL is identical to omitting lib.loc — safe to resolve
        let code = r#"source(system.file("x.R", package = "p", lib.loc = NULL))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }

    #[test]
    fn test_source_system_file_fsep_default_accepted() {
        // fsep = "/" is the default — no-op, safe to resolve
        let code = r#"source(system.file("x.R", package = "p", fsep = "/"))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        let sf = sources[0].system_file.as_ref().unwrap();
        assert_eq!(sf.parts, vec!["x.R"]);
        assert_eq!(sf.package, "p");
    }
}

// ============================================================================
// Property-Based Tests for Library Call Detection
// ============================================================================

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use tree_sitter::Parser;

    /// Parse R source code into a tree-sitter `Tree`.
    ///
    /// # Examples
    ///
    /// ```
    /// let code = "x <- 1";
    /// let tree = parse_r(code);
    /// // root node kind for R source files is "source"
    /// assert_eq!(tree.root_node().kind(), "source");
    /// ```
    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    /// R reserved words that cannot be used as package names
    const R_RESERVED: &[&str] = &[
        "if", "else", "for", "in", "while", "repeat", "next", "break", "function", "NA", "NaN",
        "Inf", "NULL", "TRUE", "FALSE", "T", "F",
    ];

    /// Determine whether a string is a valid R package name (non-empty and not an R reserved word).
    ///
    /// # Returns
    ///
    /// `true` if the name is non-empty and not an R reserved word, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(is_valid_package_name("dplyr"));
    /// assert!(!is_valid_package_name(""));
    /// assert!(!is_valid_package_name("if")); // reserved word
    /// ```
    fn is_valid_package_name(name: &str) -> bool {
        !R_RESERVED.contains(&name) && !name.is_empty()
    }

    /// Strategy that generates valid R package names.
    ///
    /// The produced strings start with a lowercase ASCII letter, contain only
    /// lowercase ASCII letters, digits, and dots, and are at most 9 characters long.
    /// Reserved or otherwise invalid package names are excluded by the strategy's filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// proptest!(|(name in crate::package_name())| {
    ///     let first = name.chars().next().unwrap();
    ///     assert!(first.is_ascii_lowercase());
    ///     assert!(name.len() <= 9);
    ///     assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'));
    /// });
    /// ```
    fn package_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9\\.]{0,8}".prop_filter("not reserved", |s| is_valid_package_name(s))
    }

    /// Produces a proptest strategy that yields one of the strings `"library"`, `"require"`, or `"loadNamespace"`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = crate::library_function();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let tree = strat.new_tree(&mut runner).unwrap();
    /// let value = tree.current();
    /// assert!(value == "library" || value == "require" || value == "loadNamespace");
    /// ```
    fn library_function() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("library"), Just("require"), Just("loadNamespace"),]
    }

    /// Generate a quote style for package names
    #[derive(Debug, Clone, Copy)]
    enum QuoteStyle {
        None,   // library(dplyr)
        Double, // library("dplyr")
        Single, // library('dplyr')
    }

    /// Produces a proptest Strategy that yields one of the `QuoteStyle` variants.
    ///
    /// The strategy generates `QuoteStyle::None`, `QuoteStyle::Double`, or `QuoteStyle::Single`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// // generate a single value from the strategy
    /// let mut runner = TestRunner::default();
    /// let value = quote_style().new_tree(&mut runner).unwrap().current();
    /// match value {
    ///     QuoteStyle::None | QuoteStyle::Double | QuoteStyle::Single => (),
    /// }
    /// ```
    fn quote_style() -> impl Strategy<Value = QuoteStyle> {
        prop_oneof![
            Just(QuoteStyle::None),
            Just(QuoteStyle::Double),
            Just(QuoteStyle::Single),
        ]
    }

    /// A library call specification for code generation
    #[derive(Debug, Clone)]
    struct LibraryCallSpec {
        func: &'static str,
        package: String,
        quote_style: QuoteStyle,
        use_named_arg: bool,
    }

    /// Generates an arbitrary `LibraryCallSpec` strategy for property-based tests.
    ///
    /// The strategy produces tuples describing a library-like call: the function name (`library`, `require`, or `loadNamespace`),
    /// a package name, the string quote style to use (none, single, or double), and a boolean indicating whether the package
    /// is supplied with a named `package=` argument.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let tree = library_call_spec().new_tree(&mut runner).unwrap();
    /// let spec = tree.current();
    /// // `spec` contains generated fields: `func`, `package`, `quote_style`, and `use_named_arg`.
    /// assert!(!spec.package.is_empty());
    /// ```
    fn library_call_spec() -> impl Strategy<Value = LibraryCallSpec> {
        (
            library_function(),
            package_name(),
            quote_style(),
            any::<bool>(),
        )
            .prop_map(
                |(func, package, quote_style, use_named_arg)| LibraryCallSpec {
                    func,
                    package,
                    quote_style,
                    use_named_arg,
                },
            )
    }

    /// Render an R library-like call from a specification.
    ///
    /// Constructs an R call string using the specification's function name, package
    /// name, quoting style, and whether to use a named `package =` argument.
    ///
    /// # Returns
    ///
    /// A `String` containing the R expression (for example `library("dplyr")` or
    /// `require(package = 'pkg')`).
    ///
    /// # Examples
    ///
    /// ```
    /// let spec = LibraryCallSpec {
    ///     func: "library".into(),
    ///     package: "dplyr".into(),
    ///     quote_style: QuoteStyle::Double,
    ///     use_named_arg: false,
    /// };
    /// let code = generate_library_call_code(&spec);
    /// assert_eq!(code, "library(\"dplyr\")");
    /// ```
    fn generate_library_call_code(spec: &LibraryCallSpec) -> String {
        let quoted_pkg = match spec.quote_style {
            QuoteStyle::None => spec.package.clone(),
            QuoteStyle::Double => format!("\"{}\"", spec.package),
            QuoteStyle::Single => format!("'{}'", spec.package),
        };

        if spec.use_named_arg {
            format!("{}(package = {})", spec.func, quoted_pkg)
        } else {
            format!("{}({})", spec.func, quoted_pkg)
        }
    }

    /// Generates a proptest strategy that yields R source text containing 1 to 5 `library`/`require`/`loadNamespace` calls interleaved with simple filler statements, together with the corresponding specs for each library call.
    ///
    /// The returned strategy produces a tuple `(code, specs)` where `code` is the generated R code as a single string (lines joined with `\n`) and `specs` is a vector of `LibraryCallSpec` describing each generated library call in document order.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use proptest::strategy::Strategy;
    /// // `r_code_with_library_calls` yields `(String, Vec<LibraryCallSpec>)`
    /// let strat = r_code_with_library_calls();
    /// // Use the strategy in a proptest test or sample from it via a test runner.
    /// let _ = strat;
    /// ```
    fn r_code_with_library_calls() -> impl Strategy<Value = (String, Vec<LibraryCallSpec>)> {
        // Generate 1-5 library calls
        prop::collection::vec(library_call_spec(), 1..=5)
            .prop_flat_map(|specs| {
                // Generate 0-3 filler lines between each call
                let num_fillers = specs.len() + 1;
                let filler_counts = prop::collection::vec(0..4usize, num_fillers);
                (Just(specs), filler_counts)
            })
            .prop_map(|(specs, filler_counts)| {
                let mut lines = Vec::new();

                // Add filler before first call
                for _ in 0..filler_counts[0] {
                    lines.push("x <- 1".to_string());
                }

                // Add library calls with fillers between them
                for (i, spec) in specs.iter().enumerate() {
                    lines.push(generate_library_call_code(spec));

                    // Add filler after this call
                    if i + 1 < filler_counts.len() {
                        for _ in 0..filler_counts[i + 1] {
                            lines.push("y <- 2".to_string());
                        }
                    }
                }

                let code = lines.join("\n");
                (code, specs)
            })
    }

    #[test]
    fn pacman_p_load_detects_qualified_and_conditional_bare_calls() {
        let code = r#"
pacman::p_load(dplyr, "ggplot2")
library(pacman)
p_load(tidyr)
"#;
        let calls = detect_library_calls(&parse_r(code), code);
        let pacman_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.package != "pacman")
            .map(|call| {
                (
                    call.package.as_str(),
                    call.requires_attached.as_deref(),
                    call.attaches,
                )
            })
            .collect();
        assert_eq!(
            pacman_calls,
            vec![
                ("dplyr", None, true),
                ("ggplot2", None, true),
                ("tidyr", Some("pacman"), true),
            ]
        );
    }

    #[test]
    fn position_before_library_call_handles_same_line_and_line_boundaries() {
        let call = |line, column| LibraryCall {
            package: "pkg".to_string(),
            line,
            column,
            function_scope: None,
            attaches: true,
            requires_attached: None,
        };

        assert_eq!(position_before_library_call(&call(4, 9)), (4, 8));
        assert_eq!(position_before_library_call(&call(4, 0)), (3, u32::MAX));
        assert_eq!(position_before_library_call(&call(0, 0)), (0, 0));
    }

    #[test]
    fn pacman_p_load_char_overrides_dots_and_resolves_static_vectors() {
        let code = r#"
packages <- c("dplyr", "tidyr")
pacman::p_load(dynamic_call(), ignored = stop("unused"), char = packages, install = FALSE)
"#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["dplyr", "tidyr"]);
    }

    #[test]
    fn pacman_p_load_still_visits_evaluated_named_controls() {
        let code = r#"pacman::p_load(char = { library(controlpkg); dynamic_packages() })"#;
        let packages: Vec<_> = detect_library_calls(&parse_r(code), code)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["controlpkg"]);
    }

    #[test]
    fn malformed_p_load_duplicate_controls_do_not_execute_values() {
        let code = r#"pacman::p_load(char = library(fakepkg), char = "realpkg")"#;
        assert!(
            detect_library_calls(&parse_r(code), code).is_empty(),
            "duplicate exact formals fail argument matching before either value executes"
        );
    }

    #[test]
    fn pacman_p_load_rejects_dynamic_and_shadowed_bare_calls() {
        for code in [
            "pacman::p_load(dplyr, package_name())",
            "pacman::p_load(dplyr, character.only = TRUE)",
            "p_load <- function(...) NULL\np_load(dplyr)",
            "f <- function(p_load) p_load(dplyr)",
        ] {
            assert!(
                detect_library_calls(&parse_r(code), code).is_empty(),
                "unexpected p_load detection for:\n{code}"
            );
        }
    }

    #[test]
    fn pacman_p_load_preserves_its_unevaluated_dots_boundary() {
        let code = "pacman::p_load({ library(should_not_run); dplyr })";
        assert!(
            detect_library_calls(&parse_r(code), code).is_empty(),
            "nested library syntax in p_load dots must not be treated as executed"
        );

        let shadowed = "p_load <- identity\np_load(library(ordinary_argument))";
        let packages: Vec<_> = detect_library_calls(&parse_r(shadowed), shadowed)
            .into_iter()
            .map(|call| call.package)
            .collect();
        assert_eq!(packages, vec!["ordinary_argument"]);
    }

    #[test]
    fn extract_attached_packages_gates_bare_p_load_in_order() {
        assert!(
            extract_attached_packages("p_load(dplyr)").is_empty(),
            "a generic bare p_load must not attach packages"
        );
        let attached = extract_attached_packages("library(pacman)\np_load(dplyr, \"tidyr\")");
        assert_eq!(
            attached,
            ["dplyr", "pacman", "tidyr"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: package-function-awareness, Property 1: Library Call Detection Completeness
        // **Validates: Requirements 1.1, 1.4, 1.5, 1.8**
        //
        // For any R source file containing library(), require(), or loadNamespace() calls
        // with static string package names, the Library_Call_Detector SHALL detect all
        // such calls and return them in document order with correct package names and positions.
        // ============================================================================

        /// Property 1: All library/require/loadNamespace calls with static package names are detected
        #[test]
        fn prop_library_call_detection_completeness((code, specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // 1. All calls should be detected (completeness)
            prop_assert_eq!(
                detected.len(),
                specs.len(),
                "Expected {} library calls, but detected {}. Code:\n{}",
                specs.len(),
                detected.len(),
                code
            );

            // 2. Package names should be correctly extracted
            for (i, (detected_call, spec)) in detected.iter().zip(specs.iter()).enumerate() {
                prop_assert_eq!(
                    &detected_call.package,
                    &spec.package,
                    "Package name mismatch at index {}. Expected '{}', got '{}'. Code:\n{}",
                    i,
                    spec.package,
                    detected_call.package,
                    code
                );
            }
        }

        /// Property 1 extended: Calls are returned in document order (sorted by line, then column)
        #[test]
        fn prop_library_calls_in_document_order((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // Verify document order: each call should be at same or later position than previous
            for i in 1..detected.len() {
                let prev = &detected[i - 1];
                let curr = &detected[i];

                let prev_pos = (prev.line, prev.column);
                let curr_pos = (curr.line, curr.column);

                prop_assert!(
                    prev_pos <= curr_pos,
                    "Library calls not in document order: call {} at ({}, {}) comes after call {} at ({}, {}). Code:\n{}",
                    i - 1, prev.line, prev.column,
                    i, curr.line, curr.column,
                    code
                );
            }
        }

        /// Property 1 extended: Positions are valid (within code bounds)
        #[test]
        fn prop_library_call_positions_valid((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            let line_count = code.lines().count() as u32;

            for (i, call) in detected.iter().enumerate() {
                // Line should be within bounds
                prop_assert!(
                    call.line < line_count,
                    "Library call {} has invalid line {}, but code only has {} lines. Code:\n{}",
                    i, call.line, line_count, code
                );

                // Column should be within the line's length (in UTF-16 code units)
                if let Some(line_text) = code.lines().nth(call.line as usize) {
                    let line_len_utf16: u32 = line_text.encode_utf16().count() as u32;
                    prop_assert!(
                        call.column <= line_len_utf16,
                        "Library call {} has invalid column {} on line {}, but line only has {} UTF-16 code units. Line: '{}'. Code:\n{}",
                        i, call.column, call.line, line_len_utf16, line_text, code
                    );
                }
            }
        }

        /// Property 1 extended: Detection is idempotent (same input produces same output)
        #[test]
        fn prop_library_call_detection_idempotent((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected1 = detect_library_calls(&tree, &code);
            let detected2 = detect_library_calls(&tree, &code);

            prop_assert_eq!(
                detected1.len(),
                detected2.len(),
                "Detection not idempotent: first call returned {} results, second returned {}",
                detected1.len(),
                detected2.len()
            );

            for (i, (d1, d2)) in detected1.iter().zip(detected2.iter()).enumerate() {
                prop_assert_eq!(
                    &d1.package, &d2.package,
                    "Detection not idempotent at index {}: package names differ",
                    i
                );
                prop_assert_eq!(
                    d1.line, d2.line,
                    "Detection not idempotent at index {}: lines differ",
                    i
                );
                prop_assert_eq!(
                    d1.column, d2.column,
                    "Detection not idempotent at index {}: columns differ",
                    i
                );
            }
        }

        /// Property 1 extended: library(), require(), and loadNamespace() are all detected
        #[test]
        fn prop_all_library_functions_detected(pkg in package_name()) {
            // Test all three function types
            let code_library = format!("library({})", pkg);
            let code_require = format!("require({})", pkg);
            let code_loadns = format!("loadNamespace({})", pkg);

            let tree_library = parse_r(&code_library);
            let tree_require = parse_r(&code_require);
            let tree_loadns = parse_r(&code_loadns);

            let detected_library = detect_library_calls(&tree_library, &code_library);
            let detected_require = detect_library_calls(&tree_require, &code_require);
            let detected_loadns = detect_library_calls(&tree_loadns, &code_loadns);

            // All should detect exactly one call
            prop_assert_eq!(detected_library.len(), 1, "library() not detected");
            prop_assert_eq!(detected_require.len(), 1, "require() not detected");
            prop_assert_eq!(detected_loadns.len(), 1, "loadNamespace() not detected");

            // All should extract the correct package name
            prop_assert_eq!(&detected_library[0].package, &pkg, "library() package name mismatch");
            prop_assert_eq!(&detected_require[0].package, &pkg, "require() package name mismatch");
            prop_assert_eq!(&detected_loadns[0].package, &pkg, "loadNamespace() package name mismatch");
        }

        /// Property 1 extended: Both quoted and unquoted package names are detected
        #[test]
        fn prop_quoted_and_unquoted_detected(pkg in package_name()) {
            let code_bare = format!("library({})", pkg);
            let code_double = format!("library(\"{}\")", pkg);
            let code_single = format!("library('{}')", pkg);

            let tree_bare = parse_r(&code_bare);
            let tree_double = parse_r(&code_double);
            let tree_single = parse_r(&code_single);

            let detected_bare = detect_library_calls(&tree_bare, &code_bare);
            let detected_double = detect_library_calls(&tree_double, &code_double);
            let detected_single = detect_library_calls(&tree_single, &code_single);

            // All should detect exactly one call
            prop_assert_eq!(detected_bare.len(), 1, "Bare identifier not detected");
            prop_assert_eq!(detected_double.len(), 1, "Double-quoted string not detected");
            prop_assert_eq!(detected_single.len(), 1, "Single-quoted string not detected");

            // All should extract the correct package name
            prop_assert_eq!(&detected_bare[0].package, &pkg, "Bare identifier package name mismatch");
            prop_assert_eq!(&detected_double[0].package, &pkg, "Double-quoted package name mismatch");
            prop_assert_eq!(&detected_single[0].package, &pkg, "Single-quoted package name mismatch");
        }

        /// Property 1 extended: Named package= argument is detected
        #[test]
        fn prop_named_package_argument_detected(pkg in package_name()) {
            let code_named = format!("library(package = {})", pkg);
            let code_named_quoted = format!("library(package = \"{}\")", pkg);

            let tree_named = parse_r(&code_named);
            let tree_named_quoted = parse_r(&code_named_quoted);

            let detected_named = detect_library_calls(&tree_named, &code_named);
            let detected_named_quoted = detect_library_calls(&tree_named_quoted, &code_named_quoted);

            // Both should detect exactly one call
            prop_assert_eq!(detected_named.len(), 1, "Named bare argument not detected");
            prop_assert_eq!(detected_named_quoted.len(), 1, "Named quoted argument not detected");

            // Both should extract the correct package name
            prop_assert_eq!(&detected_named[0].package, &pkg, "Named bare package name mismatch");
            prop_assert_eq!(&detected_named_quoted[0].package, &pkg, "Named quoted package name mismatch");
        }

        // ============================================================================
        // Feature: package-function-awareness, Property 2: Dynamic Package Name Exclusion
        // **Validates: Requirements 1.6, 1.7**
        //
        // For any R source file containing library calls with variable or expression
        // package names (including character.only = TRUE), the Library_Call_Detector
        // SHALL NOT include those calls in the detected results.
        // ============================================================================

        /// Property 2: Calls with character.only = TRUE are NOT detected
        #[test]
        fn prop_character_only_true_excluded(pkg in package_name()) {
            // Test character.only = TRUE (full form)
            let code_true = format!("library(\"{}\", character.only = TRUE)", pkg);
            let tree_true = parse_r(&code_true);
            let detected_true = detect_library_calls(&tree_true, &code_true);

            prop_assert_eq!(
                detected_true.len(),
                0,
                "library() with character.only = TRUE should NOT be detected. Code: {}",
                code_true
            );

            // Test character.only = T (short form)
            let code_t = format!("library(\"{}\", character.only = T)", pkg);
            let tree_t = parse_r(&code_t);
            let detected_t = detect_library_calls(&tree_t, &code_t);

            prop_assert_eq!(
                detected_t.len(),
                0,
                "library() with character.only = T should NOT be detected. Code: {}",
                code_t
            );
        }

        /// Property 2: require() with character.only = TRUE is NOT detected
        #[test]
        fn prop_require_character_only_true_excluded(pkg in package_name()) {
            // Test require with character.only = TRUE
            let code_true = format!("require(\"{}\", character.only = TRUE)", pkg);
            let tree_true = parse_r(&code_true);
            let detected_true = detect_library_calls(&tree_true, &code_true);

            prop_assert_eq!(
                detected_true.len(),
                0,
                "require() with character.only = TRUE should NOT be detected. Code: {}",
                code_true
            );

            // Test require with character.only = T
            let code_t = format!("require(\"{}\", character.only = T)", pkg);
            let tree_t = parse_r(&code_t);
            let detected_t = detect_library_calls(&tree_t, &code_t);

            prop_assert_eq!(
                detected_t.len(),
                0,
                "require() with character.only = T should NOT be detected. Code: {}",
                code_t
            );
        }

        /// Property 2: Calls with expression package names are NOT detected
        #[test]
        fn prop_expression_package_names_excluded(pkg in package_name()) {
            // Test paste0() expression
            let code_paste0 = format!("library(paste0(\"{}\", \"\"))", pkg);
            let tree_paste0 = parse_r(&code_paste0);
            let detected_paste0 = detect_library_calls(&tree_paste0, &code_paste0);

            prop_assert_eq!(
                detected_paste0.len(),
                0,
                "library() with paste0() expression should NOT be detected. Code: {}",
                code_paste0
            );

            // Test paste() expression
            let code_paste = format!("library(paste(\"{}\", sep = \"\"))", pkg);
            let tree_paste = parse_r(&code_paste);
            let detected_paste = detect_library_calls(&tree_paste, &code_paste);

            prop_assert_eq!(
                detected_paste.len(),
                0,
                "library() with paste() expression should NOT be detected. Code: {}",
                code_paste
            );

            // Test sprintf() expression
            let code_sprintf = format!("library(sprintf(\"%s\", \"{}\"))", pkg);
            let tree_sprintf = parse_r(&code_sprintf);
            let detected_sprintf = detect_library_calls(&tree_sprintf, &code_sprintf);

            prop_assert_eq!(
                detected_sprintf.len(),
                0,
                "library() with sprintf() expression should NOT be detected. Code: {}",
                code_sprintf
            );
        }

        /// Property 2: Calls with get() expression are NOT detected
        #[test]
        fn prop_get_expression_excluded(pkg in package_name()) {
            // Test get() expression
            let code_get = format!("library(get(\"{}\"))", pkg);
            let tree_get = parse_r(&code_get);
            let detected_get = detect_library_calls(&tree_get, &code_get);

            prop_assert_eq!(
                detected_get.len(),
                0,
                "library() with get() expression should NOT be detected. Code: {}",
                code_get
            );
        }

        /// Property 2: character.only = FALSE does NOT exclude the call
        #[test]
        fn prop_character_only_false_not_excluded(pkg in package_name()) {
            // Test character.only = FALSE (should still be detected)
            let code_false = format!("library(\"{}\", character.only = FALSE)", pkg);
            let tree_false = parse_r(&code_false);
            let detected_false = detect_library_calls(&tree_false, &code_false);

            prop_assert_eq!(
                detected_false.len(),
                1,
                "library() with character.only = FALSE SHOULD be detected. Code: {}",
                code_false
            );
            prop_assert_eq!(
                &detected_false[0].package,
                &pkg,
                "Package name mismatch for character.only = FALSE case"
            );

            // Test character.only = F (should still be detected)
            let code_f = format!("library(\"{}\", character.only = F)", pkg);
            let tree_f = parse_r(&code_f);
            let detected_f = detect_library_calls(&tree_f, &code_f);

            prop_assert_eq!(
                detected_f.len(),
                1,
                "library() with character.only = F SHOULD be detected. Code: {}",
                code_f
            );
            prop_assert_eq!(
                &detected_f[0].package,
                &pkg,
                "Package name mismatch for character.only = F case"
            );
        }
    }

    // ============================================================================
    // Property 2 Extended: Dynamic Package Exclusion with Mixed Code
    // ============================================================================

    /// Types of dynamic library calls that should be excluded
    #[derive(Debug, Clone)]
    enum DynamicCallType {
        /// character.only = TRUE
        CharacterOnlyTrue,
        /// character.only = T
        CharacterOnlyT,
        /// paste0() expression
        Paste0Expression,
        /// paste() expression
        PasteExpression,
        /// get() expression
        GetExpression,
        /// sprintf() expression
        SprintfExpression,
        /// c() expression (vector of packages)
        CExpression,
    }

    /// Creates a proptest strategy that yields one of the `DynamicCallType` variants representing
    /// dynamic/library-call patterns that should be treated as non-statically-determinable.
    ///
    /// # Returns
    ///
    /// A `Strategy` that produces a `DynamicCallType` chosen from:
    /// `CharacterOnlyTrue`, `CharacterOnlyT`, `Paste0Expression`, `PasteExpression`,
    /// `GetExpression`, `SprintfExpression`, and `CExpression`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// // obtain the strategy and use it in a proptest
    /// let strat = crate::dynamic_call_type();
    ///
    /// proptest!(|(kind in strat)| {
    ///     // `kind` is a `DynamicCallType` variant
    ///     match kind {
    ///         crate::DynamicCallType::CharacterOnlyTrue => {},
    ///         crate::DynamicCallType::CharacterOnlyT => {},
    ///         _ => {},
    ///     }
    /// });
    /// ```
    fn dynamic_call_type() -> impl Strategy<Value = DynamicCallType> {
        prop_oneof![
            Just(DynamicCallType::CharacterOnlyTrue),
            Just(DynamicCallType::CharacterOnlyT),
            Just(DynamicCallType::Paste0Expression),
            Just(DynamicCallType::PasteExpression),
            Just(DynamicCallType::GetExpression),
            Just(DynamicCallType::SprintfExpression),
            Just(DynamicCallType::CExpression),
        ]
    }

    /// Generate R code representing a dynamic (non-statically-determinable) library/require/loadNamespace call.
    ///
    /// This returns an R expression string that uses a dynamic form of specifying the package (e.g., `character.only = TRUE`, `paste0(...)`, `get(...)`, `c(...)`, etc.), which should not be recognized as a statically determinable package by static analyzers.
    /// - `call_type`: selects which dynamic pattern to emit.
    /// - `pkg`: the package name used within the generated expression (may be split or wrapped depending on the pattern).
    /// - `func`: the function name to call (e.g., `"library"`, `"require"`, or `"loadNamespace"`).
    ///
    /// # Examples
    ///
    /// ```
    /// let s = generate_dynamic_library_call(&DynamicCallType::CharacterOnlyTrue, "dplyr", "library");
    /// assert!(s.contains("character.only"));
    /// let s2 = generate_dynamic_library_call(&DynamicCallType::Paste0Expression, "pkgname", "require");
    /// assert!(s2.contains("paste0"));
    /// ```
    fn generate_dynamic_library_call(call_type: &DynamicCallType, pkg: &str, func: &str) -> String {
        match call_type {
            DynamicCallType::CharacterOnlyTrue => {
                format!("{}(\"{}\", character.only = TRUE)", func, pkg)
            }
            DynamicCallType::CharacterOnlyT => {
                format!("{}(\"{}\", character.only = T)", func, pkg)
            }
            DynamicCallType::Paste0Expression => {
                // Split package name for paste0
                let mid = pkg.len() / 2;
                let (p1, p2) = pkg.split_at(mid);
                format!("{}(paste0(\"{}\", \"{}\"))", func, p1, p2)
            }
            DynamicCallType::PasteExpression => {
                format!("{}(paste(\"{}\", sep = \"\"))", func, pkg)
            }
            DynamicCallType::GetExpression => {
                format!("{}(get(\"{}\"))", func, pkg)
            }
            DynamicCallType::SprintfExpression => {
                format!("{}(sprintf(\"%s\", \"{}\"))", func, pkg)
            }
            DynamicCallType::CExpression => {
                // c() returns a vector, not a valid single package name
                format!("{}(c(\"{}\", \"other\"))", func, pkg)
            }
        }
    }

    /// A specification for a dynamic library call
    #[derive(Debug, Clone)]
    struct DynamicLibraryCallSpec {
        func: &'static str,
        package: String,
        call_type: DynamicCallType,
    }

    /// Creates a proptest strategy that generates `DynamicLibraryCallSpec` values.
    ///
    /// The produced spec combines a library-related function variant, a package name, and a dynamic call type.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strategy = dynamic_library_call_spec();
    /// proptest!(|(spec in strategy)| {
    ///     // `spec` is a generated DynamicLibraryCallSpec
    ///     let _pkg: String = spec.package.clone();
    /// });
    /// ```
    fn dynamic_library_call_spec() -> impl Strategy<Value = DynamicLibraryCallSpec> {
        (library_function(), package_name(), dynamic_call_type()).prop_map(
            |(func, package, call_type)| DynamicLibraryCallSpec {
                func,
                package,
                call_type,
            },
        )
    }

    /// Generate R source code containing dynamic library calls that should not be detected.
    ///
    /// The returned proptest Strategy produces tuples of `(code, specs)` where `code` is the generated
    /// R source as a `String` and `specs` is a `Vec<DynamicLibraryCallSpec>` describing each dynamic
    /// library-style call inserted into the code. The code includes 1–5 dynamic calls with optional
    /// filler lines between them.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = r_code_with_dynamic_library_calls();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let (code, specs) = strat.new_tree(&mut runner).unwrap().current();
    /// assert!(!code.is_empty());
    /// assert!(!specs.is_empty());
    /// ```
    fn r_code_with_dynamic_library_calls()
    -> impl Strategy<Value = (String, Vec<DynamicLibraryCallSpec>)> {
        // Generate 1-5 dynamic library calls
        prop::collection::vec(dynamic_library_call_spec(), 1..=5)
            .prop_flat_map(|specs| {
                // Generate 0-2 filler lines between each call
                let num_fillers = specs.len() + 1;
                let filler_counts = prop::collection::vec(0..3usize, num_fillers);
                (Just(specs), filler_counts)
            })
            .prop_map(|(specs, filler_counts)| {
                let mut lines = Vec::new();

                // Add filler before first call
                for _ in 0..filler_counts[0] {
                    lines.push("x <- 1".to_string());
                }

                // Add dynamic library calls with fillers between them
                for (i, spec) in specs.iter().enumerate() {
                    lines.push(generate_dynamic_library_call(
                        &spec.call_type,
                        &spec.package,
                        spec.func,
                    ));

                    // Add filler after this call
                    if i + 1 < filler_counts.len() {
                        for _ in 0..filler_counts[i + 1] {
                            lines.push("y <- 2".to_string());
                        }
                    }
                }

                let code = lines.join("\n");
                (code, specs)
            })
    }

    /// Generate R source text containing an interleaved sequence of statically determinable
    /// and dynamic library-style calls, along with the specifications used to build them.
    ///
    /// The produced strategy yields a tuple containing:
    /// 1. A single String with the generated R code (multiple lines, interleaved static and dynamic calls).
    /// 2. A Vec of `LibraryCallSpec` describing the statically determinable calls included.
    /// 3. A Vec of `DynamicLibraryCallSpec` describing the dynamic calls included.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain the proptest strategy and use it in property tests.
    /// let strat = r_code_with_mixed_library_calls();
    /// // `strat` is a `Strategy` that generates `(String, Vec<LibraryCallSpec>, Vec<DynamicLibraryCallSpec>)`.
    /// let _ = strat;
    /// ```
    fn r_code_with_mixed_library_calls()
    -> impl Strategy<Value = (String, Vec<LibraryCallSpec>, Vec<DynamicLibraryCallSpec>)> {
        (
            prop::collection::vec(library_call_spec(), 1..=3),
            prop::collection::vec(dynamic_library_call_spec(), 1..=3),
        )
            .prop_map(|(static_specs, dynamic_specs)| {
                let mut lines = Vec::new();

                // Interleave static and dynamic calls
                let max_len = static_specs.len().max(dynamic_specs.len());
                for i in 0..max_len {
                    if i < static_specs.len() {
                        lines.push(generate_library_call_code(&static_specs[i]));
                    }
                    if i < dynamic_specs.len() {
                        lines.push(generate_dynamic_library_call(
                            &dynamic_specs[i].call_type,
                            &dynamic_specs[i].package,
                            dynamic_specs[i].func,
                        ));
                    }
                }

                let code = lines.join("\n");
                (code, static_specs, dynamic_specs)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: package-function-awareness, Property 2: Dynamic Package Name Exclusion
        // **Validates: Requirements 1.6, 1.7**
        // ============================================================================

        /// Property 2: All dynamic library calls are excluded from detection
        #[test]
        fn prop_dynamic_package_exclusion((code, _specs) in r_code_with_dynamic_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // No dynamic calls should be detected
            prop_assert_eq!(
                detected.len(),
                0,
                "Dynamic library calls should NOT be detected. Found {} calls. Code:\n{}",
                detected.len(),
                code
            );
        }

        /// Property 2: Mixed code correctly detects only static calls
        #[test]
        fn prop_mixed_static_dynamic_detection((code, static_specs, _dynamic_specs) in r_code_with_mixed_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // Only static calls should be detected
            prop_assert_eq!(
                detected.len(),
                static_specs.len(),
                "Expected {} static library calls, but detected {}. Code:\n{}",
                static_specs.len(),
                detected.len(),
                code
            );

            // Verify detected packages match static specs
            let detected_packages: std::collections::HashSet<_> = detected.iter().map(|c| &c.package).collect();
            let expected_packages: std::collections::HashSet<_> = static_specs.iter().map(|s| &s.package).collect();

            prop_assert_eq!(
                detected_packages,
                expected_packages,
                "Detected packages don't match expected static packages. Code:\n{}",
                code
            );
        }

        /// Property 2: character.only with variable value is still excluded
        #[test]
        fn prop_character_only_with_variable_excluded(pkg in package_name()) {
            // When character.only is set to a variable (not TRUE/T/FALSE/F),
            // we can't statically determine if it's true, so we should still
            // detect the call (conservative approach - only exclude TRUE/T)
            let code = format!("library(\"{}\", character.only = my_var)", pkg);
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // This should be detected because character.only is not TRUE/T
            prop_assert_eq!(
                detected.len(),
                1,
                "library() with character.only = variable SHOULD be detected (conservative). Code: {}",
                code
            );
        }
    }
}
