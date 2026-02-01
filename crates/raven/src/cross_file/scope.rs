//
// cross_file/scope.rs
//
// Scope resolution for cross-file awareness
//

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;
use tree_sitter::{Node, Tree};

use super::source_detect::{detect_library_calls, detect_rm_calls, detect_source_calls};
use super::types::{byte_offset_to_utf16_column, ForwardSource};

// ============================================================================
// Position and Interval Types for Interval Tree
// ============================================================================

/// A 2D position in a document (line, column)
/// Uses lexicographic ordering: line first, then column (Requirement 4.1)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl Position {
    /// Creates a new Position with the specified line and column.
    ///
    /// # Examples
    ///
    /// ```
    /// let p = Position::new(3, 5);
    /// assert_eq!(p.line, 3);
    /// assert_eq!(p.column, 5);
    /// ```
    pub fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Create an EOF sentinel Position used to represent end-of-file.
    ///
    /// The returned Position has its line and column set to the maximum `u32` value
    /// and is recognized by `Position::is_eof()`.
    ///
    /// # Examples
    ///
    /// ```
    /// let p = Position::eof();
    /// assert!(p.is_eof());
    /// ```
    pub fn eof() -> Self {
        Self {
            line: u32::MAX,
            column: u32::MAX,
        }
    }

    /// Indicates whether this position is the EOF sentinel (any MAX component).
    ///
    /// The EOF sentinel is represented by either the line or column being set to `u32::MAX`.
    ///
    /// # Returns
    ///
    /// `true` if the position is the EOF sentinel, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// let p = Position::eof();
    /// assert!(p.is_eof());
    ///
    /// let normal = Position::new(0, 0);
    /// assert!(!normal.is_eof());
    /// ```
    pub fn is_eof(&self) -> bool {
        self.line == u32::MAX || self.column == u32::MAX
    }

    /// Check if this is a full EOF sentinel position (both line and column are MAX)
    pub fn is_full_eof(&self) -> bool {
        self.line == u32::MAX && self.column == u32::MAX
    }
}

/// A function scope interval with start and end positions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionScopeInterval {
    pub start: Position,
    pub end: Position,
}

impl FunctionScopeInterval {
    /// Creates a function scope interval from the given start and end positions.
    ///
    /// The interval is inclusive of both `start` and `end`.
    ///
    /// # Examples
    ///
    /// ```
    /// let start = Position::new(1, 0);
    /// let end = Position::new(10, 0);
    /// let interval = FunctionScopeInterval::new(start, end);
    /// assert_eq!(interval.as_tuple(), (1, 0, 10, 0));
    /// ```
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Checks whether the interval includes the given position (inclusive).
    ///
    /// Returns `true` if `pos` lies between `start` and `end` (inclusive), `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// let interval = FunctionScopeInterval::new(Position::new(1, 0), Position::new(3, 5));
    /// assert!(interval.contains(Position::new(1, 0)));
    /// assert!(interval.contains(Position::new(2, 10)));
    /// assert!(interval.contains(Position::new(3, 5)));
    /// assert!(!interval.contains(Position::new(4, 0)));
    /// ```
    pub fn contains(&self, pos: Position) -> bool {
        self.start <= pos && pos <= self.end
    }

    /// Create a function-scope interval from a 4-tuple of positions:
    /// (start_line, start_column, end_line, end_column).
    ///
    /// # Examples
    ///
    /// ```
    /// let iv = FunctionScopeInterval::from_tuple((1, 2, 3, 4));
    /// assert_eq!(iv.as_tuple(), (1, 2, 3, 4));
    /// ```
    pub fn from_tuple(tuple: (u32, u32, u32, u32)) -> Self {
        Self {
            start: Position::new(tuple.0, tuple.1),
            end: Position::new(tuple.2, tuple.3),
        }
    }

    /// Produce a 4-tuple representing the interval boundaries for backward compatibility.
    ///
    /// # Returns
    ///
    /// `(u32, u32, u32, u32)` where the elements are `(start_line, start_column, end_line, end_column)`.
    ///
    /// # Examples
    ///
    /// ```
    /// let interval = FunctionScopeInterval::new(Position::new(1, 2), Position::new(3, 4));
    /// assert_eq!(interval.as_tuple(), (1, 2, 3, 4));
    /// ```
    pub fn as_tuple(self) -> (u32, u32, u32, u32) {
        (
            self.start.line,
            self.start.column,
            self.end.line,
            self.end.column,
        )
    }
}

// ============================================================================
// Interval Tree for Function Scope Queries
// ============================================================================

/// Node in the interval tree (internal structure)
#[derive(Debug, Clone)]
struct IntervalNode {
    /// The interval stored at this node
    interval: FunctionScopeInterval,
    /// Maximum end position in this subtree (for pruning during queries)
    max_end: Position,
    /// Left subtree (intervals with smaller start positions)
    left: Option<Box<IntervalNode>>,
    /// Right subtree (intervals with larger start positions)
    right: Option<Box<IntervalNode>>,
}

/// Interval tree for efficient function scope queries
/// 
/// This data structure enables O(log n + k) point queries where n is the number
/// of intervals and k is the number of results, compared to O(n) for linear scans.
#[derive(Debug, Clone)]
pub struct FunctionScopeTree {
    /// Root node of the tree
    root: Option<Box<IntervalNode>>,
    /// Number of intervals in the tree
    count: usize,
}

impl FunctionScopeTree {
    /// Create an empty tree
    pub fn new() -> Self {
        Self { root: None, count: 0 }
    }

    /// Indicates whether the interval tree contains no intervals.
    ///
    /// # Examples
    ///
    /// ```
    /// let empty = FunctionScopeTree::new();
    /// assert!(empty.is_empty());
    ///
    /// let nonempty = FunctionScopeTree::from_scopes(&[(0, 0, 1, 0)]);
    /// assert!(!nonempty.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Number of intervals stored in the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// let tree = FunctionScopeTree::new();
    /// assert_eq!(tree.len(), 0);
    /// ```
    pub fn len(&self) -> usize {
        self.count
    }

    /// Constructs a balanced FunctionScopeTree from a slice of function-scope tuples.
    ///
    /// Invalid intervals (where a start position is after its end) are omitted with a warning.
    /// The input is sorted and organized to produce a tree that provides efficient point queries.
    ///
    /// Time complexity: O(n log n) for sorting plus O(n) for tree construction.
    ///
    /// # Examples
    ///
    /// ```
    /// let scopes = &[ (0, 0, 10, 0), (2, 0, 5, 0), (6, 0, 9, 0) ];
    /// let tree = FunctionScopeTree::from_scopes(scopes);
    /// assert_eq!(tree.len(), 3);
    /// // Query a point inside the second interval
    /// let pos = Position::new(3, 0);
    /// let matches = tree.query_point(pos);
    /// assert!(matches.iter().any(|iv| iv.contains(pos)));
    /// ```
    ///
    /// A balanced FunctionScopeTree containing the valid intervals from `scopes`.
    pub fn from_scopes(scopes: &[(u32, u32, u32, u32)]) -> Self {
        // Convert tuples to intervals and filter out invalid ones
        let mut intervals: Vec<FunctionScopeInterval> = scopes
            .iter()
            .filter_map(|&tuple| {
                let interval = FunctionScopeInterval::from_tuple(tuple);
                // Filter out invalid intervals where start > end
                if interval.start.line > interval.end.line
                    || (interval.start.line == interval.end.line
                        && interval.start.column > interval.end.column)
                {
                    log::warn!(
                        "Filtering out invalid interval: start ({}, {}) > end ({}, {})",
                        interval.start.line,
                        interval.start.column,
                        interval.end.line,
                        interval.end.column
                    );
                    None
                } else {
                    Some(interval)
                }
            })
            .collect();

        // Handle empty case
        if intervals.is_empty() {
            return Self::new();
        }

        // Sort by start position for balanced tree construction
        intervals.sort_by_key(|interval| interval.start);

        let count = intervals.len();
        let root = Self::build_balanced_tree(&intervals);

        Self { root, count }
    }

    /// Builds a balanced interval subtree from a sorted slice of function-scope intervals.
    ///
    /// Returns the root `IntervalNode` for the slice, or `None` when the slice is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// // Construct a tree from sorted scope tuples via the public constructor.
    /// let tree = FunctionScopeTree::from_scopes(&[(0, 0, 1, 0), (2, 0, 3, 0)]);
    /// assert!(!tree.is_empty());
    /// assert_eq!(tree.len(), 2);
    /// ```
    fn build_balanced_tree(intervals: &[FunctionScopeInterval]) -> Option<Box<IntervalNode>> {
        if intervals.is_empty() {
            return None;
        }

        // Pick median element as root for balance
        let mid = intervals.len() / 2;
        let interval = intervals[mid];

        // Recursively build left and right subtrees
        let left = Self::build_balanced_tree(&intervals[..mid]);
        let right = Self::build_balanced_tree(&intervals[mid + 1..]);

        // Compute max_end as max of: node's end, left subtree max_end, right subtree max_end
        let mut max_end = interval.end;
        if let Some(ref left_node) = left {
            if left_node.max_end > max_end {
                max_end = left_node.max_end;
            }
        }
        if let Some(ref right_node) = right {
            if right_node.max_end > max_end {
                max_end = right_node.max_end;
            }
        }

        Some(Box::new(IntervalNode {
            interval,
            max_end,
            left,
            right,
        }))
    }

    /// Finds all function scope intervals that contain the given position (inclusive).
    ///
    /// The search returns every interval whose start <= `pos` <= end. If no intervals contain
    /// `pos` an empty vector is returned.
    ///
    /// # Returns
    ///
    /// A `Vec<FunctionScopeInterval>` with all intervals that contain `pos`; empty if none.
    ///
    /// # Examples
    ///
    /// ```
    /// let tree = FunctionScopeTree::from_scopes(&[(0, 0, 2, 0), (1, 0, 3, 0)]);
    /// let pos = Position::new(1, 5);
    /// let mut intervals = tree.query_point(pos);
    /// intervals.sort_by_key(|i| i.start.line); // order not guaranteed
    /// assert_eq!(intervals.len(), 2);
    /// ```
    pub fn query_point(&self, pos: Position) -> Vec<FunctionScopeInterval> {
        let mut results = Vec::new();
        if let Some(ref root) = self.root {
            Self::query_point_recursive(root, pos, &mut results);
        }
        results
    }

    /// Collects all function-scope intervals that contain a given position by traversing the interval tree.
    ///
    /// This recursive helper visits the current node and, using the node `max_end` augmentation
    /// and start-ordering invariants, prunes subtrees that cannot contain the query position
    /// to avoid unnecessary work.
    ///
    /// # Examples
    ///
    /// ```
    /// // Build a tree from scope tuples and query for intervals containing a position.
    /// let scopes = vec![(1, 0, 10, 0), (2, 0, 5, 0), (6, 0, 9, 0)];
    /// let tree = FunctionScopeTree::from_scopes(&scopes);
    /// let pos = Position::new(3, 0);
    /// let intervals = tree.query_point(pos);
    /// assert!(intervals.iter().any(|i| i.contains(pos)));
    /// ```
    fn query_point_recursive(
        node: &IntervalNode,
        pos: Position,
        results: &mut Vec<FunctionScopeInterval>,
    ) {
        // Check if current node's interval contains the position
        if node.interval.contains(pos) {
            results.push(node.interval);
        }

        // Prune left subtree if its max_end < pos
        // If the maximum end position in the left subtree is less than the query position,
        // no interval in the left subtree can contain the position.
        if let Some(ref left) = node.left {
            if left.max_end >= pos {
                Self::query_point_recursive(left, pos, results);
            }
        }

        // Prune right subtree if node's start > pos
        // Since the tree is sorted by start position, if the current node's start is greater
        // than the query position, all nodes in the right subtree also have start > pos,
        // so none of them can contain the position.
        if let Some(ref right) = node.right {
            if node.interval.start <= pos {
                Self::query_point_recursive(right, pos, results);
            }
        }
    }

    /// Selects the innermost function-scope interval that contains a given position.
    ///
    /// The "innermost" interval is the containing interval whose `start` position is
    /// lexicographically largest (latest start), corresponding to the most deeply
    /// nested function scope. Time complexity is O(log n) for balanced trees.
    /// # Returns
    ///
    /// `Some(FunctionScopeInterval)` whose start is the lexicographically largest among
    /// intervals containing `pos`, or `None` if no interval contains `pos`.
    ///
    /// # Examples
    ///
    /// ```
    /// let tree = FunctionScopeTree::from_scopes(&[
    ///     (0, 0, 10, 0), // outer scope
    ///     (2, 0, 5, 0),  // inner scope
    /// ]);
    /// let pos = Position::new(3, 0);
    /// let innermost = tree.query_innermost(pos).unwrap();
    /// assert_eq!(innermost.as_tuple(), (2, 0, 5, 0));
    /// ```
    pub fn query_innermost(&self, pos: Position) -> Option<FunctionScopeInterval> {
        if let Some(ref root) = self.root {
            Self::query_innermost_recursive(root, pos)
        } else {
            None
        }
    }

    /// Recursive helper for innermost query that finds the interval with maximum start position.
    fn query_innermost_recursive(
        node: &IntervalNode,
        pos: Position,
    ) -> Option<FunctionScopeInterval> {
        // If the current node starts after the position, only the left subtree might contain pos.
        if node.interval.start > pos {
            if let Some(ref left) = node.left {
                if left.max_end >= pos {
                    return Self::query_innermost_recursive(left, pos);
                }
            }
            return None;
        }

        // Prefer right subtree first (larger start positions).
        if let Some(ref right) = node.right {
            if right.max_end >= pos {
                if let Some(right_result) = Self::query_innermost_recursive(right, pos) {
                    return Some(right_result);
                }
            }
        }

        if node.interval.contains(pos) {
            return Some(node.interval);
        }

        if let Some(ref left) = node.left {
            if left.max_end >= pos {
                return Self::query_innermost_recursive(left, pos);
            }
        }

        None
    }
}

impl Default for FunctionScopeTree {
    /// Creates an empty function scope interval tree.
    ///
    /// # Examples
    ///
    /// ```
    /// let tree = FunctionScopeTree::default();
    /// assert!(tree.is_empty());
    /// ```
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Symbol and Scope Types
// ============================================================================

/// Symbol kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Variable,
    Parameter,
}

/// A symbol with its definition location
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub source_uri: Url,
    /// 0-based line of definition
    pub defined_line: u32,
    /// 0-based UTF-16 column of definition
    pub defined_column: u32,
    pub signature: Option<String>,
}

impl Hash for ScopedSymbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.kind.hash(state);
        self.source_uri.hash(state);
        self.defined_line.hash(state);
        self.defined_column.hash(state);
    }
}

/// A scope-introducing event within a file
#[derive(Debug, Clone)]
pub enum ScopeEvent {
    /// A symbol definition at a specific position
    Def {
        line: u32,
        column: u32,
        symbol: ScopedSymbol,
    },
    /// A source() call that introduces symbols from another file
    Source {
        line: u32,
        column: u32,
        source: ForwardSource,
    },
    /// A function definition that introduces parameter scope
    FunctionScope {
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
        parameters: Vec<ScopedSymbol>,
    },
    /// A removal of symbols from scope via rm()/remove()
    Removal {
        line: u32,
        column: u32,
        symbols: Vec<String>,
        function_scope: Option<(u32, u32, u32, u32)>,
    },
    /// A package load that introduces symbols from a package
    PackageLoad {
        line: u32,
        column: u32,
        /// Package name
        package: String,
        /// Function scope if inside a function (None = global)
        function_scope: Option<FunctionScopeInterval>,
    },
}

/// Per-file scope artifacts
#[derive(Debug, Clone)]
pub struct ScopeArtifacts {
    /// Exported interface (all symbols defined in this file)
    pub exported_interface: HashMap<String, ScopedSymbol>,
    /// Timeline of scope events in document order
    pub timeline: Vec<ScopeEvent>,
    /// Hash of exported interface for change detection
    pub interface_hash: u64,
    /// Interval tree for O(log n) function scope queries
    pub function_scope_tree: FunctionScopeTree,
}

impl Default for ScopeArtifacts {
    /// Creates an empty ScopeArtifacts with all fields set to their defaults.
    ///
    /// The produced value has an empty exported interface and timeline, an interface hash of `0`,
    /// and a default `FunctionScopeTree`.
    ///
    /// # Examples
    ///
    /// ```
    /// let artifacts = ScopeArtifacts::default();
    /// assert!(artifacts.exported_interface.is_empty());
    /// assert!(artifacts.timeline.is_empty());
    /// assert_eq!(artifacts.interface_hash, 0);
    /// ```
    fn default() -> Self {
        Self {
            exported_interface: HashMap::new(),
            timeline: Vec::new(),
            interface_hash: 0,
            function_scope_tree: FunctionScopeTree::default(),
        }
    }
}

/// Computed scope at a position
#[derive(Debug, Clone, Default)]
pub struct ScopeAtPosition {
    pub symbols: HashMap<String, ScopedSymbol>,
    pub chain: Vec<Url>,
    /// URIs where max depth was exceeded, with the source call position (line, col)
    pub depth_exceeded: Vec<(Url, u32, u32)>,
    /// Packages inherited from parent files (loaded before the source() call site)
    /// These packages are available from position (0, 0) in the child file.
    /// Requirements 5.1, 5.2, 5.3: Cross-file package propagation
    pub inherited_packages: Vec<String>,
    /// Packages loaded locally in the current file before the query position.
    /// Combined with inherited_packages, this gives all packages available at the position.
    /// Requirements 8.1, 8.3: Position-aware package loading for diagnostics
    pub loaded_packages: Vec<String>,
}

/// Determine whether a `source()` call should use local scoping rules.
///
/// The function returns `true` when the `ForwardSource` explicitly requests local
/// scoping (`local = true`) or when it represents a `sys.source` call that does
/// not target the global environment (`is_sys_source = true` and
/// `sys_source_global_env = false`).
///
/// # Examples
///
/// ```
/// let s = ForwardSource {
///     local: true,
///     is_sys_source: false,
///     sys_source_global_env: false,
/// };
/// assert!(should_apply_local_scoping(&s));
///
/// let t = ForwardSource {
///     local: false,
///     is_sys_source: true,
///     sys_source_global_env: false,
/// };
/// assert!(should_apply_local_scoping(&t));
///
/// let u = ForwardSource {
///     local: false,
///     is_sys_source: true,
///     sys_source_global_env: true,
/// };
/// assert!(!should_apply_local_scoping(&u));
/// ```
fn should_apply_local_scoping(source: &ForwardSource) -> bool {
    source.local || (source.is_sys_source && !source.sys_source_global_env)
}
/// Finds the innermost function-scope interval that contains the given position.
///
/// Given a 0-based (line, column) position, returns the containing function scope whose start is the latest (innermost) among all intervals that include the position.
///
/// # Examples
///
/// ```
/// let tree = FunctionScopeTree::from_scopes(&[(0, 0, 10, 0), (2, 0, 5, 0)]);
/// let tuple = find_containing_function_scope(&tree, 3, 1);
/// assert_eq!(tuple, Some((2, 0, 5, 0)));
/// ```
fn find_containing_function_scope(
    tree: &FunctionScopeTree,
    line: u32,
    column: u32,
) -> Option<(u32, u32, u32, u32)> {
    tree.query_innermost(Position::new(line, column))
        .map(|interval| interval.as_tuple())
}
/// Remove the given symbols from a computed scope when the removal applies.
///
/// If `removal_scope` is `None`, this removes all listed `symbols` from `scope.symbols`.
/// If `removal_scope` is `Some(scope)` the removal is applied only when that scope is
/// present in `active_function_scopes`; otherwise the call is a no-op.
///
/// # Examples
///
/// ```no_run
/// use std::collections::HashMap;
/// // Illustrative example (types elided for brevity):
/// // let mut scope = ScopeAtPosition { symbols: HashMap::new(), chain: vec![], depth_exceeded: vec![] };
/// // scope.symbols.insert("x".to_string(), /* ScopedSymbol */);
/// // apply_removal(&mut scope, &[], None, &["x".to_string()]);
/// // assert!(!scope.symbols.contains_key("x"));
/// ```
fn apply_removal(
    scope: &mut ScopeAtPosition,
    active_function_scopes: &[(u32, u32, u32, u32)],
    removal_scope: Option<(u32, u32, u32, u32)>,
    symbols: &[String],
) {
    match removal_scope {
        None => {
            for sym in symbols {
                scope.symbols.remove(sym);
            }
        }
        Some(rm_scope) if active_function_scopes.contains(&rm_scope) => {
            for sym in symbols {
                scope.symbols.remove(sym);
            }
        }
        _ => {}
    }
}

/// Build scope artifacts for a source file by extracting definitions, source() calls, and removals.
///
/// The returned ScopeArtifacts contains a document-ordered timeline of scope events (definitions,
/// source calls, function-scope entries, and removal events), an interval tree of function scopes
/// for efficient position queries, and a deterministic hash of the file's exported interface.
///
/// The function:
/// - collects symbol and function-scope definitions from the AST,
/// - records detected `source()` and `rm()/remove()` calls as timeline events,
/// - sorts the timeline by position,
/// - constructs a FunctionScopeTree from discovered function scopes and annotates removal events
///   with their containing function scope (if any),
/// - computes the exported-interface hash.
///
/// # Examples
///
/// ```ignore
/// // Given a parser-produced `tree`, file `content`, and `uri`:
/// let artifacts = compute_artifacts(&uri, &tree, content);
/// // `artifacts.timeline` contains the extracted scope events in source order.
/// assert!(artifacts.interface_hash != 0 || artifacts.exported_interface.is_empty());
/// ```
pub fn compute_artifacts(uri: &Url, tree: &Tree, content: &str) -> ScopeArtifacts {
    let mut artifacts = ScopeArtifacts::default();
    let root = tree.root_node();

    // Collect definitions from AST
    collect_definitions(root, content, uri, &mut artifacts);

    // Collect source() calls and add them to timeline.
    // Note: even when local=TRUE (or sys.source targets a non-global env), the symbols can still
    // be in-scope within a function body after the call site, so we keep these events and apply
    // scoping rules later during resolution.
    let source_calls = detect_source_calls(tree, content);
    for source in source_calls {
        artifacts.timeline.push(ScopeEvent::Source {
            line: source.line,
            column: source.column,
            source,
        });
    }

    // Collect rm()/remove() calls and add them to timeline.
    // These events will be processed during scope resolution to remove symbols from scope.
    let rm_calls = detect_rm_calls(tree, content);
    for rm_call in rm_calls {
        artifacts.timeline.push(ScopeEvent::Removal {
            line: rm_call.line,
            column: rm_call.column,
            symbols: rm_call.symbols,
            function_scope: None,
        });
    }

    // Collect library()/require()/loadNamespace() calls for later processing.
    // We need to wait until the function scope tree is built to determine function_scope.
    // (Requirements 14.2, 14.4)
    let library_calls = detect_library_calls(tree, content);

    // Sort timeline by position for correct ordering
    artifacts.timeline.sort_by_key(|event| match event {
        ScopeEvent::Def { line, column, .. } => (*line, *column),
        ScopeEvent::Source { line, column, .. } => (*line, *column),
        ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
        ScopeEvent::Removal { line, column, .. } => (*line, *column),
        ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
    });

    // Build interval tree from function scopes for O(log n) queries
    let function_scope_tuples: Vec<(u32, u32, u32, u32)> = artifacts.timeline.iter()
        .filter_map(|e| {
            if let ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, .. } = e {
                Some((*start_line, *start_column, *end_line, *end_column))
            } else {
                None
            }
        })
        .collect();
    artifacts.function_scope_tree = FunctionScopeTree::from_scopes(&function_scope_tuples);
    for event in &mut artifacts.timeline {
        if let ScopeEvent::Removal { line, column, function_scope, .. } = event {
            *function_scope = find_containing_function_scope(&artifacts.function_scope_tree, *line, *column);
        }
    }

    // Add PackageLoad events for library calls with function_scope determined from the tree.
    // (Requirements 14.2, 14.4)
    for lib_call in library_calls {
        let function_scope = find_containing_function_scope(
            &artifacts.function_scope_tree,
            lib_call.line,
            lib_call.column,
        ).map(FunctionScopeInterval::from_tuple);

        artifacts.timeline.push(ScopeEvent::PackageLoad {
            line: lib_call.line,
            column: lib_call.column,
            package: lib_call.package,
            function_scope,
        });
    }

    // Re-sort timeline to include PackageLoad events in correct position
    artifacts.timeline.sort_by_key(|event| match event {
        ScopeEvent::Def { line, column, .. } => (*line, *column),
        ScopeEvent::Source { line, column, .. } => (*line, *column),
        ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
        ScopeEvent::Removal { line, column, .. } => (*line, *column),
        ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
    });

    // Extract package names from PackageLoad events for interface hash computation
    // (Requirement 14.5: interface hash must include loaded packages for cache invalidation)
    let loaded_packages: Vec<String> = artifacts
        .timeline
        .iter()
        .filter_map(|event| {
            if let ScopeEvent::PackageLoad { package, .. } = event {
                Some(package.clone())
            } else {
                None
            }
        })
        .collect();

    // Compute interface hash including both symbols and loaded packages
    artifacts.interface_hash = compute_interface_hash(&artifacts.exported_interface, &loaded_packages);

    artifacts
}

/// Compute the lexical scope at the given document position within a single file.
///
/// The returned ScopeAtPosition reflects only local, in-file visibility at (line, column):
/// - Global definitions that occur at or before the query position are included.
/// - Function-local definitions are included only when the query position lies inside the same function scope as the definition.
/// - Function parameters are included when the query position is inside the function body (EOF sentinel positions are ignored).
/// - Removal events that occur strictly before the query position are applied, respecting function-scoped removals.
/// - Package load events from the same file are recorded in `loaded_packages` when they occur at or before the query position and match function-scoping rules.
///
/// # Examples
///
/// ```
/// let artifacts = ScopeArtifacts::default();
/// let scope = scope_at_position(&artifacts, 0, 0);
/// assert!(scope.symbols.is_empty());
/// ```
pub fn scope_at_position(
    artifacts: &ScopeArtifacts,
    line: u32,
    column: u32,
) -> ScopeAtPosition {
    let mut scope = ScopeAtPosition::default();

    // Use interval tree for O(log n) query instead of linear scan
    let is_full_eof_position = Position::new(line, column).is_full_eof();
    let active_function_scopes: Vec<(u32, u32, u32, u32)> = if is_full_eof_position {
        Vec::new()
    } else {
        artifacts.function_scope_tree
            .query_point(Position::new(line, column))
            .into_iter()
            .map(|interval| interval.as_tuple())
            .collect()
    };

    // Process events and apply function scope filtering
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                // Include if definition is before or at the position
                if (*def_line, *def_col) <= (line, column) {
                    // Use interval tree for O(log n) innermost scope lookup
                    let def_function_scope = artifacts.function_scope_tree
                        .query_innermost(Position::new(*def_line, *def_col))
                        .map(|interval| interval.as_tuple());

                    match def_function_scope {
                        None => {
                            // Global definition - always include
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { .. } => {
                // Source events are handled by scope_at_position_with_deps
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                // Skip EOF sentinel positions to avoid matching all functions
                if !is_full_eof_position && (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
                    }
                }
            }
            ScopeEvent::Removal { line: rm_line, column: rm_col, symbols, function_scope } => {
                // Only process if removal is strictly before the query position
                if (*rm_line, *rm_col) < (line, column) {
                    apply_removal(&mut scope, &active_function_scopes, *function_scope, symbols);
                }
            }
            ScopeEvent::PackageLoad { line: pkg_line, column: pkg_col, package, function_scope } => {
                // Requirements 8.1, 8.3: Position-aware package loading
                // Populate loaded_packages for callers to check package exports
                if (*pkg_line, *pkg_col) <= (line, column) {
                    // Check function scope compatibility
                    let should_include = match function_scope {
                        None => true, // Global package load - always include
                        Some(pkg_scope) => {
                            // Function-scoped package load - only include if query is in same function
                            active_function_scopes.iter().any(|active_scope| {
                                active_scope.0 == pkg_scope.start.line &&
                                active_scope.1 == pkg_scope.start.column &&
                                active_scope.2 == pkg_scope.end.line &&
                                active_scope.3 == pkg_scope.end.column
                            })
                        }
                    };
                    
                    if should_include && !scope.loaded_packages.contains(package) {
                        scope.loaded_packages.push(package.clone());
                    }
                }
            }
        }
    }

    scope
}

/// Compute the lexical scope at a document position, including package exports
/// loaded before that position.
///
/// This function returns the set of symbols visible at (line, column) in a
/// single file by processing the file's timeline (definitions, function
/// scopes, removals, and PackageLoad events). Package exports are injected
/// using the provided `get_package_exports` callback for each package loaded
/// at or before the query position; `base_exports` are seeded with lowest
/// precedence and may be overridden by local definitions or explicit package
/// loads. Function-scoped package loads are only visible when the query
/// position lies inside the corresponding function scope.
///
/// # Examples
///
/// ```
/// use std::collections::HashSet;
/// let artifacts = crate::cross_file::scope::ScopeArtifacts::default();
/// let get_exports = |_pkg: &str| -> HashSet<String> { HashSet::new() };
/// let base_exports = HashSet::new();
/// let scope = crate::cross_file::scope::scope_at_position_with_packages(&artifacts, 0, 0, &get_exports, &base_exports);
/// assert!(scope.symbols.is_empty());
/// ```
pub fn scope_at_position_with_packages<F>(
    artifacts: &ScopeArtifacts,
    line: u32,
    column: u32,
    get_package_exports: &F,
    base_exports: &HashSet<String>,
) -> ScopeAtPosition
where
    F: Fn(&str) -> HashSet<String>,
{
    let mut scope = ScopeAtPosition::default();

    // Requirements 6.3, 6.4: Base packages are always available at all positions
    // without requiring explicit library() calls and without position-aware loading.
    // Add base exports first with lowest precedence - they will be overridden by
    // local definitions and explicit package loads via entry().or_insert_with().
    let base_uri = Url::parse("package:base").unwrap_or_else(|_| Url::parse("package:unknown").unwrap());
    for export_name in base_exports {
        scope.symbols.insert(
            export_name.clone(),
            ScopedSymbol {
                name: export_name.clone(),
                kind: SymbolKind::Variable, // Base exports are treated as variables
                source_uri: base_uri.clone(),
                defined_line: 0,
                defined_column: 0,
                signature: None,
            },
        );
    }

    // Use interval tree for O(log n) query instead of linear scan
    let is_full_eof_position = Position::new(line, column).is_full_eof();
    let active_function_scopes: Vec<(u32, u32, u32, u32)> = if is_full_eof_position {
        Vec::new()
    } else {
        artifacts.function_scope_tree
            .query_point(Position::new(line, column))
            .into_iter()
            .map(|interval| interval.as_tuple())
            .collect()
    };

    // Process events and apply function scope filtering
    // Note: Local definitions and explicit package loads will override base exports
    // because we use insert() for definitions (which overwrites) and entry().or_insert_with()
    // for package loads (which preserves existing entries including local definitions).
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                // Include if definition is before or at the position
                if (*def_line, *def_col) <= (line, column) {
                    // Use interval tree for O(log n) innermost scope lookup
                    let def_function_scope = artifacts.function_scope_tree
                        .query_innermost(Position::new(*def_line, *def_col))
                        .map(|interval| interval.as_tuple());

                    match def_function_scope {
                        None => {
                            // Global definition - always include
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { .. } => {
                // Source events are handled by scope_at_position_with_deps
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                // Skip EOF sentinel positions to avoid matching all functions
                if !is_full_eof_position && (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
                    }
                }
            }
            ScopeEvent::Removal { line: rm_line, column: rm_col, symbols, function_scope } => {
                // Only process if removal is strictly before the query position
                if (*rm_line, *rm_col) < (line, column) {
                    apply_removal(&mut scope, &active_function_scopes, *function_scope, symbols);
                }
            }
            ScopeEvent::PackageLoad { line: pkg_line, column: pkg_col, package, function_scope } => {
                // Process PackageLoad events for position-aware package loading
                // Requirements 2.1, 2.2: Only include if package is loaded before query position
                if (*pkg_line, *pkg_col) <= (line, column) {
                    // Requirements 2.4, 2.5: Check function scope compatibility
                    let should_include = match function_scope {
                        None => {
                            // Global package load - always available after the load position
                            true
                        }
                        Some(pkg_scope) => {
                            // Function-scoped package load - only available within that function
                            // Check if query position is inside the same function scope
                            active_function_scopes.iter().any(|active_scope| {
                                active_scope.0 == pkg_scope.start.line &&
                                active_scope.1 == pkg_scope.start.column &&
                                active_scope.2 == pkg_scope.end.line &&
                                active_scope.3 == pkg_scope.end.column
                            })
                        }
                    };

                    if should_include {
                        // Get package exports and add them to scope
                        let exports = get_package_exports(package);
                        
                        // Create a pseudo-URI for the package source
                        // This allows hover/definition to identify package symbols
                        let package_uri = Url::parse(&format!("package:{}", package))
                            .unwrap_or_else(|_| Url::parse("package:unknown").unwrap());
                        
                        for export_name in exports {
                            // Check if symbol already exists
                            let should_insert = match scope.symbols.get(&export_name) {
                                None => true, // No existing symbol, insert
                                Some(existing) => {
                                    // Only override if existing is from base package
                                    // Local definitions (non-package URIs) take precedence
                                    existing.source_uri.as_str() == "package:base"
                                }
                            };
                            
                            if should_insert {
                                scope.symbols.insert(
                                    export_name.clone(),
                                    ScopedSymbol {
                                        name: export_name,
                                        kind: SymbolKind::Variable, // Package exports are treated as variables
                                        source_uri: package_uri.clone(),
                                        defined_line: 0,
                                        defined_column: 0,
                                        signature: None,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    scope
}

/// Compute scope at a position with cross-file traversal.
/// This is the main entry point for cross-file scope resolution.
pub fn scope_at_position_with_deps<F>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
{
    log::trace!("Resolving scope at {}:{}:{}", uri, line, column);
    let mut visited = HashSet::new();
    let scope = scope_at_position_recursive(uri, line, column, get_artifacts, resolve_path, max_depth, 0, &mut visited);
    log::trace!("Found {} symbols in scope", scope.symbols.len());
    scope
}

/// Compute the lexical scope at a given position for `uri`, merging symbols from the file
/// and any transitive `source()` targets up to `max_depth`, while respecting local
/// scoping, function-local definitions, removals, and cycle prevention.
///
/// Only `source()` targets whose calls occur before the query position are followed.
/// Function-scoped sources and definitions are visible only when the query position is
/// inside the same function scope. Traversal stops when `max_depth` would be exceeded;
/// locations where traversal was curtailed are recorded in the result.
///
/// Parameters with non-obvious behavior:
/// - `get_artifacts`: callback that returns `ScopeArtifacts` for a `Url`.
/// - `resolve_path`: callback that resolves a source path relative to `uri` and returns a `Url`.
/// - `max_depth`: maximum number of recursive `source()` hops to follow (root call counts as depth 0).
/// - `visited`: mutable set used to prevent cycles across recursion; callers should provide an empty set.
///
/// # Returns
///
/// A `ScopeAtPosition` whose `symbols` are the merged, precedence-resolved symbols available at the
/// queried position; `chain` is the sequence of visited URIs (root first); `depth_exceeded` lists
/// (uri, line, column) tuples where traversal was stopped due to `max_depth`.
///
/// # Examples
///
/// ```ignore
/// use lsp_types::Url;
/// use std::collections::HashSet;
///
/// let uri = Url::parse("file:///project/main.R").unwrap();
/// let get_artifacts = |_u: &Url| -> Option<ScopeArtifacts> { None };
/// let resolve_path = |_path: &str, _base: &Url| -> Option<Url> { None };
/// let mut visited = HashSet::new();
///
/// let scope = scope_at_position_recursive(
///     &uri,
///     10, // line
///     2,  // column
///     &get_artifacts,
///     &resolve_path,
///     10, // max_depth
///     0,  // current_depth
///     &mut visited,
/// );
/// ```
#[allow(clippy::too_many_arguments)]
fn scope_at_position_recursive<F>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<Url>,
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
{
    log::trace!("Traversing to file: {} (depth {})", uri, current_depth);
    let mut scope = ScopeAtPosition::default();

    if current_depth >= max_depth || visited.contains(uri) {
        return scope;
    }
    visited.insert(uri.clone());
    scope.chain.push(uri.clone());

    let artifacts = match get_artifacts(uri) {
        Some(a) => a,
        None => {
            log::trace!("No artifacts found for {}", uri);
            return scope;
        }
    };

    // Use interval tree for O(log n) query instead of linear scan
    let is_full_eof_position = Position::new(line, column).is_full_eof();
    let active_function_scopes: Vec<(u32, u32, u32, u32)> = if is_full_eof_position {
        Vec::new()
    } else {
        artifacts.function_scope_tree
            .query_point(Position::new(line, column))
            .into_iter()
            .map(|interval| interval.as_tuple())
            .collect()
    };

    // Process timeline events up to the requested position
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Local definitions take precedence (don't overwrite)
                    // Use interval tree for O(log n) innermost scope lookup
                    let def_function_scope = artifacts.function_scope_tree
                        .query_innermost(Position::new(*def_line, *def_col))
                        .map(|interval| interval.as_tuple());
                    
                    // Skip function-local definitions not in our scope
                    if let Some(def_scope) = def_function_scope {
                        if !active_function_scopes.contains(&def_scope) {
                            continue;
                        }
                    }
                    scope.symbols.entry(symbol.name.clone()).or_insert_with(|| {
                        log::trace!("  Found symbol: {} ({})", symbol.name, match symbol.kind {
                            SymbolKind::Function => "function",
                            SymbolKind::Variable => "variable",
                            SymbolKind::Parameter => "parameter",
                        });
                        symbol.clone()
                    });
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // If this is a local-only source (or sys.source into a non-global env), only
                    // make its symbols available within the containing function scope.
                    if should_apply_local_scoping(source) {
                        // Use interval tree for O(log n) innermost scope lookup
                        let source_function_scope = artifacts.function_scope_tree
                            .query_innermost(Position::new(*src_line, *src_col))
                            .map(|interval| interval.as_tuple());

                        if let Some(src_scope) = source_function_scope {
                            if !active_function_scopes.contains(&src_scope) {
                                continue;
                            }
                        } else {
                            // local=TRUE at top-level doesn't contribute to global scope
                            continue;
                        }
                    }

                    // Resolve the path and get symbols from sourced file
                    if let Some(child_uri) = resolve_path(&source.path, uri) {
                        // Check if we would exceed max depth
                        if current_depth + 1 >= max_depth {
                            scope.depth_exceeded.push((uri.clone(), *src_line, *src_col));
                            continue;
                        }

                        let child_scope = scope_at_position_recursive(
                            &child_uri,
                            u32::MAX, // Include all symbols from sourced file
                            u32::MAX,
                            get_artifacts,
                            resolve_path,
                            max_depth,
                            current_depth + 1,
                            visited,
                        );
                        // Merge child symbols (local definitions take precedence)
                        for (name, symbol) in child_scope.symbols {
                            scope.symbols.entry(name).or_insert(symbol);
                        }
                        scope.chain.extend(child_scope.chain);
                        scope.depth_exceeded.extend(child_scope.depth_exceeded);
                    }
                }
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                // Skip EOF sentinel positions to avoid matching all functions
                if !is_full_eof_position && (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.entry(param.name.clone()).or_insert_with(|| param.clone());
                    }
                }
            }
            ScopeEvent::Removal { line: rm_line, column: rm_col, symbols, function_scope } => {
                // Only process if removal is strictly before the query position
                if (*rm_line, *rm_col) < (line, column) {
                    apply_removal(&mut scope, &active_function_scopes, *function_scope, symbols);
                }
            }
            ScopeEvent::PackageLoad { .. } => {
                // PackageLoad events are handled by scope resolution with package library access
            }
        }
    }

    log::trace!("File {} contributed {} symbols", uri, scope.symbols.len());
    scope
}

fn collect_definitions(
    node: Node,
    content: &str,
    uri: &Url,
    artifacts: &mut ScopeArtifacts,
) {
    // Check for assignment expressions
    if node.kind() == "binary_operator" {
        if let Some(symbol) = try_extract_assignment(node, content, uri) {
            let event = ScopeEvent::Def {
                line: symbol.defined_line,
                column: symbol.defined_column,
                symbol: symbol.clone(),
            };
            artifacts.timeline.push(event);
            artifacts.exported_interface.insert(symbol.name.clone(), symbol);
        }
    }
    
    // Check for assign() calls (Requirement 17.4)
    if node.kind() == "call" {
        if let Some(symbol) = try_extract_assign_call(node, content, uri) {
            let event = ScopeEvent::Def {
                line: symbol.defined_line,
                column: symbol.defined_column,
                symbol: symbol.clone(),
            };
            artifacts.timeline.push(event);
            artifacts.exported_interface.insert(symbol.name.clone(), symbol);
        }
    }

    // Check for for loop iterators
    if node.kind() == "for_statement" {
        if let Some(symbol) = try_extract_for_loop_iterator(node, content, uri) {
            let event = ScopeEvent::Def {
                line: symbol.defined_line,
                column: symbol.defined_column,
                symbol: symbol.clone(),
            };
            artifacts.timeline.push(event);
            artifacts.exported_interface.insert(symbol.name.clone(), symbol);
        }
    }

    // Check for function definitions to extract parameter scope
    if node.kind() == "function_definition" {
        if let Some(function_scope) = try_extract_function_scope(node, content, uri) {
            artifacts.timeline.push(function_scope);
        }
    }

    // Recurse into children
    for child in node.children(&mut node.walk()) {
        collect_definitions(child, content, uri, artifacts);
    }
}

/// Extract function parameter scope from function_definition nodes.
/// Creates ScopedSymbol for each parameter and determines function body boundaries.
fn try_extract_function_scope(node: Node, content: &str, uri: &Url) -> Option<ScopeEvent> {
    // tree-sitter-r node shapes have changed across versions; be robust by falling back
    // to scanning children by kind if field lookups fail.
    let params_node = node
        .child_by_field_name("parameters")
        .or_else(|| {
            node.children(&mut node.walk())
                .find(|c| c.is_named() && c.kind() == "parameters")
        })?;

    let body_node = node
        .child_by_field_name("body")
        .or_else(|| {
            // Most common body node for function definitions.
            node.children(&mut node.walk())
                .find(|c| c.is_named() && c.kind() == "braced_expression")
        })
        .or_else(|| {
            // Fallback: last named child that isn't the parameters list.
            node.children(&mut node.walk())
                .filter(|c| c.is_named() && c.id() != params_node.id())
                .last()
        })?;

    // Extract parameters
    let mut parameters = Vec::new();
    for child in params_node.children(&mut params_node.walk()) {
        // Parameters may appear as parameter, default_parameter, identifier, dots, etc.
        if matches!(child.kind(), "parameter" | "default_parameter" | "identifier" | "dots") {
            if let Some(param_symbol) = extract_parameter_symbol(child, content, uri) {
                parameters.push(param_symbol);
            }
        }
    }

    // Determine function body boundaries
    let body_start = body_node.start_position();
    let body_end = body_node.end_position();

    // Convert to UTF-16 columns
    let start_line_text = content.lines().nth(body_start.row).unwrap_or("");
    let end_line_text = content.lines().nth(body_end.row).unwrap_or("");
    let start_column = byte_offset_to_utf16_column(start_line_text, body_start.column);
    let end_column = byte_offset_to_utf16_column(end_line_text, body_end.column);

    Some(ScopeEvent::FunctionScope {
        start_line: body_start.row as u32,
        start_column,
        end_line: body_end.row as u32,
        end_column,
        parameters,
    })
}

/// Extract a parameter symbol from a parameter node
fn extract_parameter_symbol(param_node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Handle different parameter types
    match param_node.kind() {
        "parameter" | "default_parameter" => {
            // Look for identifier or dots child.
            for child in param_node.children(&mut param_node.walk()) {
                if child.kind() == "identifier" {
                    let name = node_text(child, content).to_string();
                    let start = child.start_position();
                    let line_text = content.lines().nth(start.row).unwrap_or("");
                    let column = byte_offset_to_utf16_column(line_text, start.column);

                    return Some(ScopedSymbol {
                        name,
                        kind: SymbolKind::Parameter,
                        source_uri: uri.clone(),
                        defined_line: start.row as u32,
                        defined_column: column,
                        signature: None,
                    });
                } else if child.kind() == "dots" {
                    let start = child.start_position();
                    let line_text = content.lines().nth(start.row).unwrap_or("");
                    let column = byte_offset_to_utf16_column(line_text, start.column);

                    return Some(ScopedSymbol {
                        name: "...".to_string(),
                        kind: SymbolKind::Parameter,
                        source_uri: uri.clone(),
                        defined_line: start.row as u32,
                        defined_column: column,
                        signature: None,
                    });
                }
            }
        }
        "identifier" => {
            // Direct identifier (some grammars may use this directly under parameters)
            let name = node_text(param_node, content).to_string();
            let start = param_node.start_position();
            let line_text = content.lines().nth(start.row).unwrap_or("");
            let column = byte_offset_to_utf16_column(line_text, start.column);

            return Some(ScopedSymbol {
                name,
                kind: SymbolKind::Parameter,
                source_uri: uri.clone(),
                defined_line: start.row as u32,
                defined_column: column,
                signature: None,
            });
        }
        "dots" => {
            // Handle ellipsis (...) parameter when it's the parameter node itself
            let start = param_node.start_position();
            let line_text = content.lines().nth(start.row).unwrap_or("");
            let column = byte_offset_to_utf16_column(line_text, start.column);

            return Some(ScopedSymbol {
                name: "...".to_string(),
                kind: SymbolKind::Parameter,
                source_uri: uri.clone(),
                defined_line: start.row as u32,
                defined_column: column,
                signature: None,
            });
        }
        _ => {}
    }

    None
}

/// Extract definition from assign("name", value) calls.
/// Only handles string literal names per Requirement 17.4.
fn try_extract_assign_call(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Get function name
    let func_node = node.child_by_field_name("function")?;
    let func_name = node_text(func_node, content);
    
    if func_name != "assign" {
        return None;
    }
    
    // Get arguments
    let args_node = node.child_by_field_name("arguments")?;
    
    // Find the first argument (the name)
    let mut name_arg = None;
    for child in args_node.children(&mut args_node.walk()) {
        if child.kind() == "argument" {
            // Check if it's a named argument
            if let Some(name_node) = child.child_by_field_name("name") {
                let arg_name = node_text(name_node, content);
                if arg_name == "x" {
                    // This is the name argument
                    name_arg = child.child_by_field_name("value");
                    break;
                }
            } else {
                // Positional argument - first one is the name
                name_arg = child.child_by_field_name("value");
                break;
            }
        }
    }
    
    let name_node = name_arg?;
    
    // Only handle string literals
    if name_node.kind() != "string" {
        return None;
    }
    
    // Extract the string content (remove quotes)
    let name_text = node_text(name_node, content);
    let name = name_text.trim_matches(|c| c == '"' || c == '\'').to_string();
    
    if name.is_empty() {
        return None;
    }
    
    // Get position with UTF-16 column
    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);
    
    Some(ScopedSymbol {
        name,
        kind: SymbolKind::Variable,
        source_uri: uri.clone(),
        defined_line: start.row as u32,
        defined_column: column,
        signature: None,
    })
}

/// Extract loop iterator from for_statement nodes.
/// In R, loop iterators persist after the loop completes.
fn try_extract_for_loop_iterator(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Get the variable field (iterator)
    let var_node = node.child_by_field_name("variable")?;
    
    // Only handle identifier nodes
    if var_node.kind() != "identifier" {
        return None;
    }
    
    let name = node_text(var_node, content).to_string();
    
    // Get position with UTF-16 column
    let start = var_node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);
    
    Some(ScopedSymbol {
        name,
        kind: SymbolKind::Variable,
        source_uri: uri.clone(),
        defined_line: start.row as u32,
        defined_column: column,
        signature: None,
    })
}

fn try_extract_assignment(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Check if this is an assignment operator - the operator is a direct child, not a field
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    
    if children.len() != 3 {
        return None;
    }
    
    let lhs = children[0];
    let op = children[1];
    let rhs = children[2];
    
    // Check operator
    let op_text = node_text(op, content);
    
    // Handle -> operator: RHS is the name, LHS is the value
    if op_text == "->" {
        if rhs.kind() != "identifier" {
            return None;
        }
        let name = node_text(rhs, content).to_string();
        
        let (kind, signature) = if lhs.kind() == "function_definition" {
            let sig = extract_function_signature(lhs, &name, content);
            (SymbolKind::Function, Some(sig))
        } else {
            (SymbolKind::Variable, None)
        };
        
        // Position is at RHS (the identifier being defined)
        let start = rhs.start_position();
        let line_text = content.lines().nth(start.row).unwrap_or("");
        let column = byte_offset_to_utf16_column(line_text, start.column);
        
        return Some(ScopedSymbol {
            name,
            kind,
            source_uri: uri.clone(),
            defined_line: start.row as u32,
            defined_column: column,
            signature,
        });
    }
    
    // Handle <- = <<- operators: LHS is the name, RHS is the value
    if !matches!(op_text, "<-" | "=" | "<<-") {
        return None;
    }

    // Get the left-hand side (name)
    if lhs.kind() != "identifier" {
        return None;
    }
    let name = node_text(lhs, content).to_string();

    // Get the right-hand side to determine kind
    let (kind, signature) = if rhs.kind() == "function_definition" {
        let sig = extract_function_signature(rhs, &name, content);
        (SymbolKind::Function, Some(sig))
    } else {
        (SymbolKind::Variable, None)
    };

    // Get position with UTF-16 column
    let start = lhs.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(ScopedSymbol {
        name,
        kind,
        source_uri: uri.clone(),
        defined_line: start.row as u32,
        defined_column: column,
        signature,
    })
}

fn extract_function_signature(func_node: Node, name: &str, content: &str) -> String {
    // Find the parameters node
    let mut cursor = func_node.walk();
    for child in func_node.children(&mut cursor) {
        if child.kind() == "parameters" {
            let params = node_text(child, content);
            return format!("{}{}", name, params);
        }
    }
    format!("{}()", name)
}

/// Get the source slice corresponding to a tree-sitter `Node`.
///
/// The returned string slice borrows from `content` and spans the byte range reported by `node.byte_range()`.
///
/// # Examples
///
/// ```
/// // Given a `node: tree_sitter::Node` and `source: &str`:
/// // let text = node_text(node, source);
/// // `text` will be the substring of `source` that corresponds to `node`.
/// ```
fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

/// Compute a deterministic hash of the exported interface and loaded packages.
///
/// Symbols are incorporated deterministically by sorting the interface keys before hashing each
/// ScopedSymbol; package names are included sorted as well. The resulting hash is suitable for
/// cache invalidation when a file's exported symbols or loaded packages change.
///
/// # Returns
///
/// `u64` hash of the provided `interface` and `packages`.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
/// // Use an empty interface and no packages as the simplest example.
/// let interface: HashMap<String, crate::ScopedSymbol> = HashMap::new();
/// let packages: Vec<String> = Vec::new();
/// let h1 = crate::compute_interface_hash(&interface, &packages);
/// let h2 = crate::compute_interface_hash(&interface, &packages);
/// assert_eq!(h1, h2);
/// ```
fn compute_interface_hash(
    interface: &HashMap<String, ScopedSymbol>,
    packages: &[String],
) -> u64 {
    let mut hasher = DefaultHasher::new();

    // Sort keys for deterministic hashing of symbols
    let mut keys: Vec<_> = interface.keys().collect();
    keys.sort();
    for key in keys {
        if let Some(symbol) = interface.get(key) {
            symbol.hash(&mut hasher);
        }
    }

    // Include loaded packages in the hash (sorted for determinism)
    // This ensures cache invalidation when packages change (Requirement 14.5)
    let mut sorted_packages: Vec<_> = packages.iter().collect();
    sorted_packages.sort();
    for package in sorted_packages {
        package.hash(&mut hasher);
    }

    hasher.finish()
}

/// Compute scope at a position with backward directive support.
/// This processes backward directives FIRST (parent context), then forward sources.
/// 
/// Property 19: Backward-First Resolution Order
/// - Backward directives establish parent context (symbols available before this file runs)
/// - Forward source() calls add symbols in document order
#[allow(clippy::too_many_arguments)]
pub fn scope_at_position_with_backward<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    parent_call_site: Option<(u32, u32)>, // (line, column) in parent where this file is sourced
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    let mut visited = HashSet::new();
    scope_at_position_with_backward_recursive(
        uri, line, column, get_artifacts, get_metadata, resolve_path,
        max_depth, 0, &mut visited, parent_call_site,
    )
}

/// Extended scope resolution that also uses dependency graph edges.
/// This is the preferred entry point when a DependencyGraph is available.
#[allow(clippy::too_many_arguments)]
pub fn scope_at_position_with_graph<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    graph: &super::dependency::DependencyGraph,
    workspace_root: Option<&Url>,
    max_depth: usize,
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    let mut visited = HashSet::new();
    
    // Build initial PathContext for the root file
    let meta = get_metadata(uri);
    let path_ctx = meta.as_ref()
        .and_then(|m| super::path_resolve::PathContext::from_metadata(uri, m, workspace_root))
        .or_else(|| super::path_resolve::PathContext::new(uri, workspace_root));
    
    scope_at_position_with_graph_recursive(
        uri, line, column, get_artifacts, get_metadata, graph, workspace_root,
        path_ctx, max_depth, 0, &mut visited, &[],
    )
}

/// Compute the lexical and cross-file scope visible at a position using the dependency graph.
///
/// This collects symbols visible at (line, column) in `uri` by merging parent (backward) symbols
/// from dependency-graph edges, applying the file's own timeline (definitions, parameters,
/// removals) and resolving forward `source()` calls (including propagated package loads).
/// Function-scoped visibility, sys.source/local scoping rules, cycle prevention, and `max_depth`
/// are respected; entries that would exceed `max_depth` are recorded in `ScopeAtPosition::depth_exceeded`.
///
/// # Returns
///
/// A `ScopeAtPosition` containing the merged symbols, provenance chain, recorded depth-exceeded
/// entries, and package information applicable at the queried position.
///
/// # Examples
///
/// ```
/// use url::Url;
/// use std::collections::HashSet;
///
/// // Minimal stubs: no artifacts, no metadata, and an empty graph.
/// let uri = Url::parse("file:///example.R").unwrap();
/// let get_artifacts = |_u: &Url| -> Option<super::ScopeArtifacts> { None };
/// let get_metadata = |_u: &Url| -> Option<super::types::CrossFileMetadata> { None };
/// let graph: super::dependency::DependencyGraph = Default::default();
/// let mut visited: HashSet<Url> = HashSet::new();
///
/// let scope = super::scope_at_position_with_graph_recursive(
///     &uri,
///     1,
///     1,
///     &get_artifacts,
///     &get_metadata,
///     &graph,
///     None,
///     None,
///     10,
///     0,
///     &mut visited,
///     &[], // no inherited packages
/// );
///
/// assert!(scope.symbols.is_empty());
/// ```
#[allow(clippy::too_many_arguments)]
fn scope_at_position_with_graph_recursive<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    graph: &super::dependency::DependencyGraph,
    workspace_root: Option<&Url>,
    path_ctx: Option<super::path_resolve::PathContext>,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<Url>,
    inherited_packages: &[String],
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    // Initialize scope with inherited_packages from parameter
    // Requirements 5.1, 5.2: Packages inherited from parent files are available from position (0, 0)
    let mut scope = ScopeAtPosition {
        inherited_packages: inherited_packages.to_vec(),
        ..Default::default()
    };

    if current_depth >= max_depth || visited.contains(uri) {
        return scope;
    }
    visited.insert(uri.clone());
    scope.chain.push(uri.clone());

    let artifacts = match get_artifacts(uri) {
        Some(a) => a,
        None => return scope,
    };

    // STEP 1: Process parent context from dependency graph edges
    // Get edges where this file is the child (callee)
    for edge in graph.get_dependents(uri) {
        // Skip local=TRUE edges (symbols not inherited)
        if edge.local {
            continue;
        }
        // Skip sys.source with non-global env
        if edge.is_sys_source {
            // For sys.source, we need to check if it's global env
            // The edge doesn't store this directly, so we check metadata
            if let Some(meta) = get_metadata(&edge.from) {
                let is_global = meta.sources.iter().any(|s| {
                    s.is_sys_source && s.sys_source_global_env && 
                    s.line == edge.call_site_line.unwrap_or(u32::MAX)
                });
                if !is_global {
                    continue;
                }
            }
        }

        // Get call site position for filtering
        let call_site_line = edge.call_site_line.unwrap_or(u32::MAX);
        let call_site_col = edge.call_site_column.unwrap_or(u32::MAX);

        // Check if we would exceed max depth
        if current_depth + 1 >= max_depth {
            scope.depth_exceeded.push((uri.clone(), call_site_line, call_site_col));
            continue;
        }

        // Build PathContext for parent
        let parent_meta = get_metadata(&edge.from);
        let parent_ctx = parent_meta.as_ref()
            .and_then(|m| super::path_resolve::PathContext::from_metadata(&edge.from, m, workspace_root))
            .or_else(|| super::path_resolve::PathContext::new(&edge.from, workspace_root));

        // Get parent's scope at the call site
        // Note: We pass empty inherited_packages here because the parent will collect
        // its own inherited packages from its parents via the dependency graph
        let parent_scope = scope_at_position_with_graph_recursive(
            &edge.from,
            call_site_line,
            call_site_col,
            get_artifacts,
            get_metadata,
            graph,
            workspace_root,
            parent_ctx,
            max_depth,
            current_depth + 1,
            visited,
            &[], // Parent collects its own inherited packages
        );

        // Merge parent symbols (they are available at the START of this file)
        for (name, symbol) in parent_scope.symbols {
            scope.symbols.entry(name).or_insert(symbol);
        }
        scope.chain.extend(parent_scope.chain);
        scope.depth_exceeded.extend(parent_scope.depth_exceeded);

        // Requirements 5.1, 5.2, 5.3: Propagate PackageLoad events from parent files
        // Collect packages loaded in parent before the source() call site
        // These packages are available in the child file from position (0, 0)
        if let Some(parent_artifacts) = get_artifacts(&edge.from) {
            for event in &parent_artifacts.timeline {
                if let ScopeEvent::PackageLoad { line: pkg_line, column: pkg_col, package, function_scope } = event {
                    // Only propagate packages loaded before the call site
                    // Requirement 5.1: Package loaded before source() call is available in sourced file
                    if (*pkg_line, *pkg_col) <= (call_site_line, call_site_col) {
                        // Requirement 5.3: Respect function scope - only propagate global packages
                        // or packages in the same function scope as the source() call
                        let should_propagate = match function_scope {
                            None => true, // Global package load - always propagate
                            Some(pkg_scope) => {
                                // Function-scoped package load - only propagate if the source() call
                                // is within the same function scope
                                if let Some(parent_artifacts_ref) = get_artifacts(&edge.from) {
                                    let call_site_scope = parent_artifacts_ref.function_scope_tree
                                        .query_innermost(Position::new(call_site_line, call_site_col))
                                        .map(|interval| interval.as_tuple());
                                    
                                    call_site_scope.is_some_and(|cs_scope| {
                                        cs_scope.0 == pkg_scope.start.line &&
                                        cs_scope.1 == pkg_scope.start.column &&
                                        cs_scope.2 == pkg_scope.end.line &&
                                        cs_scope.3 == pkg_scope.end.column
                                    })
                                } else {
                                    false
                                }
                            }
                        };

                        if should_propagate && !scope.inherited_packages.contains(package) {
                            scope.inherited_packages.push(package.clone());
                        }
                    }
                }
            }
        }

        // Also propagate packages that the parent inherited from its parents
        // Requirement 5.2: Inherit loaded packages from parent up to call site
        for pkg in &parent_scope.inherited_packages {
            if !scope.inherited_packages.contains(pkg) {
                scope.inherited_packages.push(pkg.clone());
            }
        }
    }

    // STEP 2: Process timeline events (local definitions and forward sources)
    // Use interval tree for O(log n) query instead of linear scan
    let is_eof_position = line == u32::MAX && column == u32::MAX;
    let active_function_scopes: Vec<(u32, u32, u32, u32)> = if is_eof_position {
        Vec::new()
    } else {
        artifacts.function_scope_tree
            .query_point(Position::new(line, column))
            .into_iter()
            .map(|interval| interval.as_tuple())
            .collect()
    };
    
    // Second pass: process events and apply function scope filtering
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Use interval tree for O(log n) innermost scope lookup
                    let def_function_scope = artifacts.function_scope_tree
                        .query_innermost(Position::new(*def_line, *def_col))
                        .map(|interval| interval.as_tuple());
                    
                    match def_function_scope {
                        None => {
                            // Global definition - always include (local definitions take precedence over inherited symbols)
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // If this is a local-only source (or sys.source into a non-global env), only
                    // make its symbols available within the containing function scope.
                    if should_apply_local_scoping(source) {
                        // Use interval tree for O(log n) innermost scope lookup
                        let source_function_scope = artifacts.function_scope_tree
                            .query_innermost(Position::new(*src_line, *src_col))
                            .map(|interval| interval.as_tuple());

                        if let Some(src_scope) = source_function_scope {
                            if !active_function_scopes.contains(&src_scope) {
                                continue;
                            }
                        } else {
                            // local=TRUE at top-level doesn't contribute to global scope
                            continue;
                        }
                    }

                    // Resolve the path using PathContext
                    let child_uri = path_ctx.as_ref().and_then(|ctx| {
                        let resolved = super::path_resolve::resolve_path(&source.path, ctx)?;
                        super::path_resolve::path_to_uri(&resolved)
                    });
                    
                    if let Some(child_uri) = child_uri {
                        // Check if we would exceed max depth
                        if current_depth + 1 >= max_depth {
                            scope.depth_exceeded.push((uri.clone(), *src_line, *src_col));
                            continue;
                        }

                        // Requirements 5.1, 5.3: Collect packages loaded before this source() call
                        // to pass to the child file. The child will have access to these packages
                        // from position (0, 0).
                        // Get the function scope of the source() call for filtering function-scoped packages
                        let source_function_scope = artifacts.function_scope_tree
                            .query_innermost(Position::new(*src_line, *src_col))
                            .map(|interval| interval.as_tuple());

                        let mut extra_packages: Vec<String> = Vec::new();

                        // Collect packages from this file's timeline that are loaded before the source() call
                        for pkg_event in &artifacts.timeline {
                            if let ScopeEvent::PackageLoad { line: pkg_line, column: pkg_col, package, function_scope } = pkg_event {
                                // Only include packages loaded before the source() call
                                if (*pkg_line, *pkg_col) < (*src_line, *src_col) {
                                    // Check function scope compatibility
                                    let should_include = match function_scope {
                                        None => true, // Global package load - always include
                                        Some(pkg_scope) => {
                                            // Function-scoped package load - only include if:
                                            // 1. The source() call is in the same function scope, OR
                                            // 2. The source() call is nested within the package's function scope
                                            source_function_scope.is_some_and(|src_scope| {
                                                src_scope.0 == pkg_scope.start.line &&
                                                src_scope.1 == pkg_scope.start.column &&
                                                src_scope.2 == pkg_scope.end.line &&
                                                src_scope.3 == pkg_scope.end.column
                                            })
                                        }
                                    };
                                    
                                    if should_include
                                        && !scope.inherited_packages.contains(package)
                                        && !extra_packages.contains(package) {
                                        extra_packages.push(package.clone());
                                    }
                                }
                            }
                        }

                        let mut packages_for_child: Vec<String>;
                        let packages_slice: &[String];

                        if extra_packages.is_empty() {
                            packages_slice = scope.inherited_packages.as_slice();
                        } else {
                            packages_for_child = scope.inherited_packages.clone();
                            for pkg in extra_packages {
                                if !packages_for_child.contains(&pkg) {
                                    packages_for_child.push(pkg);
                                }
                            }
                            packages_slice = packages_for_child.as_slice();
                        }

                        // Build child PathContext, respecting chdir flag
                        let child_path = child_uri.to_file_path().ok();
                        let child_ctx = child_path.as_ref().and_then(|cp| {
                            let ctx = path_ctx.as_ref()?;
                            // Get child's metadata for its own working directory directive
                            let child_meta = get_metadata(&child_uri);
                            if let Some(cm) = child_meta {
                                // Child has its own metadata - use it, but inherit working dir if no explicit one
                                let mut child_ctx = super::path_resolve::PathContext::from_metadata(&child_uri, &cm, workspace_root)?;
                                if child_ctx.working_directory.is_none() {
                                    // Inherit from parent based on chdir flag
                                    child_ctx.inherited_working_directory = if source.chdir {
                                        Some(cp.parent()?.to_path_buf())
                                    } else {
                                        Some(ctx.effective_working_directory())
                                    };
                                }
                                Some(child_ctx)
                            } else {
                                // No metadata for child - create context based on chdir
                                Some(ctx.child_context_for_source(cp, source.chdir))
                            }
                        });

                        let child_scope = scope_at_position_with_graph_recursive(
                            &child_uri,
                            u32::MAX, // Include all symbols from sourced file
                            u32::MAX,
                            get_artifacts,
                            get_metadata,
                            graph,
                            workspace_root,
                            child_ctx,
                            max_depth,
                            current_depth + 1,
                            visited,
                            packages_slice, // Pass inherited packages to child
                        );
                        // Merge child symbols (local definitions take precedence)
                        for (name, symbol) in child_scope.symbols {
                            scope.symbols.entry(name).or_insert(symbol);
                        }
                        scope.chain.extend(child_scope.chain);
                        scope.depth_exceeded.extend(child_scope.depth_exceeded);
                    }
                }
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                // Skip EOF sentinel positions to avoid matching all functions
                let is_eof_position = line == u32::MAX && column == u32::MAX;
                if !is_eof_position && (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
                    }
                }
            }
            ScopeEvent::Removal { line: rm_line, column: rm_col, symbols, function_scope } => {
                // Only process if removal is strictly before the query position
                if (*rm_line, *rm_col) < (line, column) {
                    apply_removal(&mut scope, &active_function_scopes, *function_scope, symbols);
                }
            }
            ScopeEvent::PackageLoad { line: pkg_line, column: pkg_col, package, function_scope } => {
                // Requirements 8.1, 8.3: Position-aware package loading
                // Only include packages loaded before the query position
                if (*pkg_line, *pkg_col) <= (line, column) {
                    // Check function scope compatibility
                    let should_include = match function_scope {
                        None => true, // Global package load - always include
                        Some(pkg_scope) => {
                            // Function-scoped package load - only include if query is in same function
                            active_function_scopes.iter().any(|active_scope| {
                                active_scope.0 == pkg_scope.start.line &&
                                active_scope.1 == pkg_scope.start.column &&
                                active_scope.2 == pkg_scope.end.line &&
                                active_scope.3 == pkg_scope.end.column
                            })
                        }
                    };
                    
                    if should_include && !scope.loaded_packages.contains(package) {
                        scope.loaded_packages.push(package.clone());
                    }
                }
            }
        }
    }

    scope
}

/// Compute the scope visible at a given position by first applying backward (parent) directives
/// and then processing this file's timeline (local definitions, forward sources, function scopes,
/// and removals), performing recursive cross-file traversal with cycle prevention and depth limits.
///
/// Parameters:
/// - `uri`: URI of the file to compute scope for.
/// - `line`, `column`: 0-based UTF-16 line and column of the query position (use `u32::MAX` to represent EOF sentinels when propagating full-file inclusion).
/// - `get_artifacts`: function that returns `ScopeArtifacts` for a given `Url` (or `None` if unavailable).
/// - `get_metadata`: function that returns cross-file metadata (backward directives) for a given `Url` (or `None`).
/// - `resolve_path`: path resolution callback that maps a source path string and the current file `Url` to an optional target `Url`.
/// - `max_depth`: maximum allowed recursion depth; when exceeded, the function records entries in `ScopeAtPosition::depth_exceeded`.
/// - `current_depth`: current recursion depth (caller-managed).
/// - `visited`: mutable set of visited URIs used to avoid cycles; the function inserts `uri` on entry.
/// - `_parent_call_site`: reserved for future use; currently unused.
///
/// Behavior notes:
/// - Backward directives (parents that source this file) are processed first and their symbols are treated
///   as available at the start of this file but have lower precedence than local definitions.
/// - Forward `source()` calls in the timeline are resolved and merged, respecting `max_depth` and visited cycles.
/// - Function parameters are included only when the query position lies within the function body.
/// - Removals are applied only for removal events that occur strictly before the query position and respect function-local scoping.
/// - The function uses the file's `FunctionScopeTree` to determine function-local vs global definitions.
///
/// # Returns
///
/// A `ScopeAtPosition` describing symbols visible at the requested position, the chain of traversed URIs,
/// and any depth-exceeded entries encountered during recursion.
///
/// # Examples
///
/// ```
/// use std::collections::HashSet;
/// use url::Url;
///
/// // Minimal example: no artifacts or metadata available -> empty scope
/// let uri = Url::parse("file:///example.R").unwrap();
/// let get_artifacts = |_u: &Url| None;
/// let get_metadata = |_u: &Url| None;
/// let resolve = |_p: &str, _u: &Url| None;
/// let mut visited = HashSet::new();
///
/// let scope = scope_at_position_with_backward_recursive(
///     &uri,
///     0,
///     0,
///     &get_artifacts,
///     &get_metadata,
///     &resolve,
///     5,    // max_depth
///     0,    // current_depth
///     &mut visited,
///     None, // parent call site
/// );
///
/// assert!(scope.symbols.is_empty());
/// ```
#[allow(clippy::too_many_arguments)]
fn scope_at_position_with_backward_recursive<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<Url>,
    _parent_call_site: Option<(u32, u32)>, // Currently unused but reserved for future use
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    let mut scope = ScopeAtPosition::default();

    if current_depth >= max_depth || visited.contains(uri) {
        return scope;
    }
    visited.insert(uri.clone());
    scope.chain.push(uri.clone());

    let artifacts = match get_artifacts(uri) {
        Some(a) => a,
        None => return scope,
    };

    // STEP 1: Process backward directives FIRST (parent context)
    // This establishes symbols that are available at the START of this file
    if let Some(metadata) = get_metadata(uri) {
        for directive in &metadata.sourced_by {
            if let Some(parent_uri) = resolve_path(&directive.path, uri) {
                // Get the call site in the parent
                let call_site = match &directive.call_site {
                    super::types::CallSiteSpec::Line(n) => Some((*n, u32::MAX)), // end of line
                    super::types::CallSiteSpec::Match(_) => {
                        // Match resolution requires content provider - not available in this path
                        // Fall back to end of file (conservative)
                        Some((u32::MAX, u32::MAX))
                    }
                    super::types::CallSiteSpec::Default => Some((u32::MAX, u32::MAX)), // end of file
                };

                if let Some((call_line, call_col)) = call_site {
                    // Check if we would exceed max depth
                    if current_depth + 1 >= max_depth {
                        scope.depth_exceeded.push((uri.clone(), directive.directive_line, 0));
                        continue;
                    }

                    // Get parent's scope at the call site
                    let parent_scope = scope_at_position_with_backward_recursive(
                        &parent_uri,
                        call_line,
                        call_col,
                        get_artifacts,
                        get_metadata,
                        resolve_path,
                        max_depth,
                        current_depth + 1,
                        visited,
                        None, // parent doesn't have a parent call site in this context
                    );

                    // Merge parent symbols (they are available at the START of this file)
                    // These have lower precedence than local definitions
                    for (name, symbol) in parent_scope.symbols {
                        scope.symbols.entry(name).or_insert(symbol);
                    }
                    scope.chain.extend(parent_scope.chain);
                    scope.depth_exceeded.extend(parent_scope.depth_exceeded);
                }
            }
        }
    }

    // STEP 2: Process timeline events (local definitions and forward sources)
    // Use interval tree for function scope queries (skip EOF sentinel)
    let query_pos = Position::new(line, column);
    let active_function_scopes: Vec<(u32, u32, u32, u32)> = if query_pos.is_full_eof() {
        Vec::new()
    } else {
        artifacts.function_scope_tree.query_point(query_pos)
            .into_iter()
            .map(|interval| interval.as_tuple())
            .collect()
    };
    
    // Second pass: process events and apply function scope filtering
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Check if this definition is inside any function scope using interval tree
                    let def_function_scope = artifacts.function_scope_tree
                        .query_innermost(Position::new(*def_line, *def_col))
                        .map(|interval| interval.as_tuple());
                    
                    match def_function_scope {
                        None => {
                            // Global definition - local definitions take precedence over inherited symbols
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // Resolve the path and get symbols from sourced file
                    if let Some(child_uri) = resolve_path(&source.path, uri) {
                        // Check if we would exceed max depth
                        if current_depth + 1 >= max_depth {
                            scope.depth_exceeded.push((uri.clone(), *src_line, *src_col));
                            continue;
                        }

                        let child_scope = scope_at_position_with_backward_recursive(
                            &child_uri,
                            u32::MAX, // Include all symbols from sourced file
                            u32::MAX,
                            get_artifacts,
                            get_metadata,
                            resolve_path,
                            max_depth,
                            current_depth + 1,
                            visited,
                            Some((*src_line, *src_col)), // pass the call site
                        );
                        // Merge child symbols (local definitions take precedence)
                        for (name, symbol) in child_scope.symbols {
                            scope.symbols.entry(name).or_insert(symbol);
                        }
                        scope.chain.extend(child_scope.chain);
                        scope.depth_exceeded.extend(child_scope.depth_exceeded);
                    }
                }
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                // Skip EOF sentinel positions to avoid matching all functions
                let is_eof_position = line == u32::MAX || column == u32::MAX;
                if !is_eof_position && (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
                    }
                }
            }
            ScopeEvent::Removal { line: rm_line, column: rm_col, symbols, function_scope } => {
                // Only process if removal is strictly before the query position
                if (*rm_line, *rm_col) < (line, column) {
                    apply_removal(&mut scope, &active_function_scopes, *function_scope, symbols);
                }
            }
            ScopeEvent::PackageLoad { line: pkg_line, column: pkg_col, package, function_scope } => {
                // Requirements 8.1, 8.3: Position-aware package loading
                // Only include packages loaded before the query position
                if (*pkg_line, *pkg_col) <= (line, column) {
                    // Check function scope compatibility
                    let should_include = match function_scope {
                        None => true, // Global package load - always include
                        Some(pkg_scope) => {
                            // Function-scoped package load - only include if query is in same function
                            active_function_scopes.iter().any(|active_scope| {
                                active_scope.0 == pkg_scope.start.line &&
                                active_scope.1 == pkg_scope.start.column &&
                                active_scope.2 == pkg_scope.end.line &&
                                active_scope.3 == pkg_scope.end.column
                            })
                        }
                    };
                    
                    if should_include && !scope.loaded_packages.contains(package) {
                        scope.loaded_packages.push(package.clone());
                    }
                }
            }
        }
    }

    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
        parser.parse(code, None).unwrap()
    }

    fn test_uri() -> Url {
        Url::parse("file:///test.R").unwrap()
    }

    #[test]
    fn test_function_definition() {
        let code = "my_func <- function(x, y) { x + y }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("my_func").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Function);
        assert_eq!(symbol.signature, Some("my_func(x, y)".to_string()));
    }

    #[test]
    fn test_variable_definition() {
        let code = "x <- 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("x").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Variable);
        assert!(symbol.signature.is_none());
    }

    #[test]
    fn test_equals_assignment() {
        let code = "x = 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        assert!(artifacts.exported_interface.contains_key("x"));
    }

    #[test]
    fn test_super_assignment() {
        let code = "x <<- 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        assert!(artifacts.exported_interface.contains_key("x"));
    }

    #[test]
    fn test_multiple_definitions() {
        let code = "x <- 1\ny <- 2\nz <- function() {}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 3);
        assert!(artifacts.exported_interface.contains_key("x"));
        assert!(artifacts.exported_interface.contains_key("y"));
        assert!(artifacts.exported_interface.contains_key("z"));
    }

    #[test]
    fn test_scope_at_position() {
        let code = "x <- 1\ny <- 2\nz <- 3";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // At line 0, only x should be in scope
        let scope = scope_at_position(&artifacts, 0, 10);
        assert!(scope.symbols.contains_key("x"));
        assert!(!scope.symbols.contains_key("y"));

        // At line 1, x and y should be in scope
        let scope = scope_at_position(&artifacts, 1, 10);
        assert!(scope.symbols.contains_key("x"));
        assert!(scope.symbols.contains_key("y"));
        assert!(!scope.symbols.contains_key("z"));

        // At line 2, all should be in scope
        let scope = scope_at_position(&artifacts, 2, 10);
        assert_eq!(scope.symbols.len(), 3);
    }


    #[test]
    fn test_interface_hash_deterministic() {
        let code = "x <- 1\ny <- 2";
        let tree = parse_r(code);
        let artifacts1 = compute_artifacts(&test_uri(), &tree, code);
        let artifacts2 = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts1.interface_hash, artifacts2.interface_hash);
    }

    #[test]
    fn test_interface_hash_changes() {
        let code1 = "x <- 1";
        let code2 = "x <- 1\ny <- 2";
        let tree1 = parse_r(code1);
        let tree2 = parse_r(code2);
        let artifacts1 = compute_artifacts(&test_uri(), &tree1, code1);
        let artifacts2 = compute_artifacts(&test_uri(), &tree2, code2);

        assert_ne!(artifacts1.interface_hash, artifacts2.interface_hash);
    }

    #[test]
    fn test_assign_call_string_literal() {
        let code = r#"assign("my_var", 42)"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("my_var").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Variable);
    }

    #[test]
    fn test_assign_call_dynamic_name_ignored() {
        let code = r#"assign(name_var, 42)"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Dynamic name should not be treated as a definition
        assert_eq!(artifacts.exported_interface.len(), 0);
    }

    #[test]
    fn test_for_loop_iterator_extraction() {
        let code = "for (i in 1:10) { print(i) }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("i").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Variable);
        assert_eq!(symbol.name, "i");
        assert!(symbol.signature.is_none());
    }

    #[test]
    fn test_for_loop_iterator_with_complex_sequence() {
        let code = "for (item in my_list) { process(item) }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        assert!(artifacts.exported_interface.contains_key("item"));
    }

    #[test]
    fn test_for_loop_iterator_persists_after_loop() {
        let code = "for (j in 1:5) { }\nresult <- j";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Both j (iterator) and result should be in scope
        assert_eq!(artifacts.exported_interface.len(), 2);
        assert!(artifacts.exported_interface.contains_key("j"));
        assert!(artifacts.exported_interface.contains_key("result"));
    }

    #[test]
    fn test_nested_for_loops() {
        let code = "for (i in 1:3) { for (j in 1:2) { print(i, j) } }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Both iterators should be in scope
        assert_eq!(artifacts.exported_interface.len(), 2);
        assert!(artifacts.exported_interface.contains_key("i"));
        assert!(artifacts.exported_interface.contains_key("j"));
    }

    #[test]
    fn test_backward_directive_call_site_filtering() {
        use crate::cross_file::types::{BackwardDirective, CallSiteSpec, CrossFileMetadata};

        let parent_uri = Url::parse("file:///parent.R").unwrap();
        let child_uri = Url::parse("file:///child.R").unwrap();

        // Parent code: a on line 0, x1 on line 1, x2 on line 2, y on line 3
        let parent_code = "a <- 1\nx1 <- 1\nx2 <- 2\ny <- 2";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Verify parent artifacts
        println!("Parent timeline:");
        for event in &parent_artifacts.timeline {
            match event {
                ScopeEvent::Def { line, column, symbol } => {
                    println!("  Def: {} at ({}, {})", symbol.name, line, column);
                }
                _ => {}
            }
        }

        // Child with backward directive line=2 (1-based, so 0-based line 1)
        let child_code = "z <- 3";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Line(1), // 0-based line 1
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "../parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // Test scope at end of child file (line 0, after z definition)
        let scope = scope_at_position_with_backward(
            &child_uri, 0, 10, &get_artifacts, &get_metadata, &resolve_path, 10, None
        );

        // Should have: a (from parent line 0), x1 (from parent line 1), z (local)
        // Should NOT have: x2 (parent line 2), y (parent line 3) - after call site
        assert!(scope.symbols.contains_key("a"), "Should have 'a' from parent");
        assert!(scope.symbols.contains_key("x1"), "Should have 'x1' from parent");
        assert!(scope.symbols.contains_key("z"), "Should have 'z' from local");
        assert!(!scope.symbols.contains_key("x2"), "Should NOT have 'x2' - after call site");
        assert!(!scope.symbols.contains_key("y"), "Should NOT have 'y' - after call site");
    }

    #[test]
    fn test_source_local_false_global_scope() {
        // Test that source() with local=FALSE makes symbols available (inherits_symbols() returns true)
        let source = ForwardSource {
            path: "child.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            local: false,  // local=FALSE
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: false,
        };

        assert!(source.inherits_symbols(), "source() with local=FALSE should inherit symbols");

        // Test that such sources are included in timeline
        let code = "x <- 1\nsource(\"child.R\", local = FALSE)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have source event in timeline
        let source_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { source, .. } => Some(source),
                _ => None,
            })
            .collect();

        assert_eq!(source_events.len(), 1, "Should have one source event");
        assert!(!source_events[0].local, "Source should have local=FALSE");
        assert!(source_events[0].inherits_symbols(), "Source should inherit symbols");
    }

    #[test]
    fn test_source_local_true_not_inherited() {
        // source(local=TRUE) does not inherit symbols into the global scope, but the call site
        // should still be represented in the timeline so scope resolution can make symbols
        // available within the containing function scope.
        let source = ForwardSource {
            path: "child.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            local: true, // local=TRUE
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: false,
        };

        assert!(!source.inherits_symbols(), "source() with local=TRUE should NOT inherit symbols");

        // Local=TRUE sources are included in the timeline
        let code = "x <- 1\nsource(\"child.R\", local = TRUE)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let source_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { source, .. } => Some(source),
                _ => None,
            })
            .collect();

        assert_eq!(source_events.len(), 1, "Should have one source event");
        assert!(source_events[0].local, "Source should have local=TRUE");

        // But local=TRUE at top-level should not make child symbols available in global scope.
        let parent_uri = Url::parse("file:///parent.R").unwrap();
        let child_uri = Url::parse("file:///child.R").unwrap();

        let child_code = "child_var <- 42";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        let scope = scope_at_position_with_deps(&parent_uri, 10, 0, &get_artifacts, &resolve_path, 10);
        assert!(!scope.symbols.contains_key("child_var"), "local=TRUE should not leak symbols to global scope");
    }

    #[test]
    fn test_source_default_local_false() {
        // Test that source() without local parameter defaults to local=FALSE behavior
        let code = "x <- 1\nsource(\"child.R\")\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have source event in timeline (defaults to local=FALSE)
        let source_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { source, .. } => Some(source),
                _ => None,
            })
            .collect();

        assert_eq!(source_events.len(), 1, "Should have one source event");
        assert!(!source_events[0].local, "Source should default to local=FALSE");
        assert!(source_events[0].inherits_symbols(), "Source should inherit symbols by default");
    }

    #[test]
    fn test_scope_at_position_with_graph() {
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: defines 'a' then sources child
        let parent_code = "a <- 1\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines 'b'
        let child_code = "b <- 2";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // At end of parent file, both 'a' and 'b' should be available
        let scope = scope_at_position_with_graph(
            &parent_uri, 10, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("a"), "a should be available");
        assert!(scope.symbols.contains_key("b"), "b should be available from sourced file");
    }

    #[test]
    fn test_scope_with_graph_parent_context() {
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: defines 'parent_var' then sources child at line 1
        let parent_code = "parent_var <- 1\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines 'child_var'
        let child_code = "child_var <- 2";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // In child file, parent_var should be available via dependency graph edge
        let scope = scope_at_position_with_graph(
            &child_uri, 10, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("parent_var"), "parent_var should be available from parent");
        assert!(scope.symbols.contains_key("child_var"), "child_var should be available locally");
    }

    #[test]
    fn test_max_depth_exceeded_forward() {
        // Test that depth_exceeded is populated when max depth is hit on forward sources
        let uri_a = Url::parse("file:///project/a.R").unwrap();
        let uri_b = Url::parse("file:///project/b.R").unwrap();
        let uri_c = Url::parse("file:///project/c.R").unwrap();

        // a.R sources b.R, b.R sources c.R
        let code_a = "source(\"b.R\")";
        let code_b = "source(\"c.R\")";
        let code_c = "x <- 1";

        let tree_a = parse_r(code_a);
        let tree_b = parse_r(code_b);
        let tree_c = parse_r(code_c);

        let artifacts_a = compute_artifacts(&uri_a, &tree_a, code_a);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, code_b);
        let artifacts_c = compute_artifacts(&uri_c, &tree_c, code_c);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else if uri == &uri_c { Some(artifacts_c.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, from: &Url| -> Option<Url> {
            if from == &uri_a && path == "b.R" { Some(uri_b.clone()) }
            else if from == &uri_b && path == "c.R" { Some(uri_c.clone()) }
            else { None }
        };

        // With max_depth=2, traversing a->b->c should exceed at b->c
        let scope = scope_at_position_with_deps(&uri_a, u32::MAX, u32::MAX, &get_artifacts, &resolve_path, 2);

        // Should have depth_exceeded entry for b.R at the source("c.R") call
        assert!(!scope.depth_exceeded.is_empty(), "depth_exceeded should not be empty");
        assert!(scope.depth_exceeded.iter().any(|(uri, _, _)| uri == &uri_b), 
            "depth_exceeded should contain b.R");
    }

    #[test]
    fn test_max_depth_exceeded_backward() {
        // Test that depth_exceeded is populated when max depth is hit on backward directives
        use super::super::dependency::DependencyGraph;
        use super::super::types::{CrossFileMetadata, ForwardSource};

        let uri_a = Url::parse("file:///project/a.R").unwrap();
        let uri_b = Url::parse("file:///project/b.R").unwrap();
        let uri_c = Url::parse("file:///project/c.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // c.R is sourced by b.R, b.R is sourced by a.R
        let code_a = "a_var <- 1\nsource(\"b.R\")";
        let code_b = "b_var <- 2\nsource(\"c.R\")";
        let code_c = "c_var <- 3";

        let tree_a = parse_r(code_a);
        let tree_b = parse_r(code_b);
        let tree_c = parse_r(code_c);

        let artifacts_a = compute_artifacts(&uri_a, &tree_a, code_a);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, code_b);
        let artifacts_c = compute_artifacts(&uri_c, &tree_c, code_c);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let meta_a = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "b.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        let meta_b = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "c.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        graph.update_file(&uri_a, &meta_a, Some(&workspace_root), |_| None);
        graph.update_file(&uri_b, &meta_b, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else if uri == &uri_c { Some(artifacts_c.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &uri_a { Some(meta_a.clone()) }
            else if uri == &uri_b { Some(meta_b.clone()) }
            else { None }
        };

        // With max_depth=2, traversing c->b->a should exceed
        let scope = scope_at_position_with_graph(
            &uri_c, u32::MAX, u32::MAX, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 2,
        );

        // Should have depth_exceeded entry
        assert!(!scope.depth_exceeded.is_empty(), "depth_exceeded should not be empty with max_depth=2");
    }

    #[test]
    fn test_lsp_source_directive_in_scope() {
        // Test that @lsp-source directives are treated as source call sites for scope resolution
        use super::super::types::ForwardSource;

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();

        // Parent file: has @lsp-source directive on line 2 (0-based: line 1)
        // The directive is parsed into sources with is_directive=true
        let parent_code = "x <- 1\n# @lsp-source child.R\ny <- 2";
        let parent_tree = parse_r(parent_code);
        let mut parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Manually add the directive source (normally done by directive parsing)
        parent_artifacts.timeline.push(ScopeEvent::Source {
            line: 1,
            column: 0,
            source: ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: true,  // This is the key - it's a directive
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            },
        });
        parent_artifacts.timeline.sort_by_key(|e| match e {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Child file: defines 'child_var'
        let child_code = "child_var <- 42";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Before the @lsp-source directive (line 0), child_var should NOT be in scope
        let scope_before = scope_at_position_with_deps(&parent_uri, 0, 10, &get_artifacts, &resolve_path, 10);
        assert!(!scope_before.symbols.contains_key("child_var"), 
            "child_var should NOT be in scope before @lsp-source directive");

        // After the @lsp-source directive (line 2), child_var SHOULD be in scope
        let scope_after = scope_at_position_with_deps(&parent_uri, 2, 0, &get_artifacts, &resolve_path, 10);
        assert!(scope_after.symbols.contains_key("child_var"), 
            "child_var SHOULD be in scope after @lsp-source directive");
    }

    #[test]
    fn test_chdir_affects_nested_path_resolution() {
        // Test that chdir=TRUE causes child's relative paths to resolve from child's directory
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        // Directory structure:
        // /project/main.R - sources data/loader.R with chdir=TRUE
        // /project/data/loader.R - sources helpers.R (relative to data/)
        // /project/data/helpers.R - defines helper_func
        let main_uri = Url::parse("file:///project/main.R").unwrap();
        let loader_uri = Url::parse("file:///project/data/loader.R").unwrap();
        let helpers_uri = Url::parse("file:///project/data/helpers.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // main.R: sources data/loader.R with chdir=TRUE
        let main_code = "x <- 1\nsource(\"data/loader.R\", chdir = TRUE)";
        let main_tree = parse_r(main_code);
        let main_artifacts = compute_artifacts(&main_uri, &main_tree, main_code);

        // loader.R: sources helpers.R (relative path)
        let loader_code = "source(\"helpers.R\")\nloader_var <- 1";
        let loader_tree = parse_r(loader_code);
        let loader_artifacts = compute_artifacts(&loader_uri, &loader_tree, loader_code);

        // helpers.R: defines helper_func
        let helpers_code = "helper_func <- function() {}";
        let helpers_tree = parse_r(helpers_code);
        let helpers_artifacts = compute_artifacts(&helpers_uri, &helpers_tree, helpers_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let main_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "data/loader.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: true, // chdir=TRUE
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        let loader_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "helpers.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        graph.update_file(&main_uri, &main_meta, Some(&workspace_root), |_| None);
        graph.update_file(&loader_uri, &loader_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &main_uri { Some(main_artifacts.clone()) }
            else if uri == &loader_uri { Some(loader_artifacts.clone()) }
            else if uri == &helpers_uri { Some(helpers_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &main_uri { Some(main_meta.clone()) }
            else if uri == &loader_uri { Some(loader_meta.clone()) }
            else { None }
        };

        // At end of main.R, helper_func should be available because:
        // 1. main.R sources data/loader.R with chdir=TRUE
        // 2. loader.R's working directory becomes /project/data/
        // 3. loader.R sources "helpers.R" which resolves to /project/data/helpers.R
        let scope = scope_at_position_with_graph(
            &main_uri, u32::MAX, u32::MAX, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("x"), "x should be available");
        assert!(scope.symbols.contains_key("loader_var"), "loader_var should be available from loader.R");
        assert!(scope.symbols.contains_key("helper_func"), "helper_func should be available from helpers.R via chdir");
    }

    #[test]
    fn test_working_directory_directive_affects_path_resolution() {
        // Test that @lsp-working-directory affects path resolution
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        // Directory structure:
        // /project/scripts/main.R - has @lsp-working-directory /data, sources helpers.R
        // /project/data/helpers.R - defines helper_func
        let main_uri = Url::parse("file:///project/scripts/main.R").unwrap();
        let helpers_uri = Url::parse("file:///project/data/helpers.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // main.R: has working directory directive, sources helpers.R
        let main_code = "# @lsp-working-directory /data\nsource(\"helpers.R\")";
        let main_tree = parse_r(main_code);
        let main_artifacts = compute_artifacts(&main_uri, &main_tree, main_code);

        // helpers.R: defines helper_func
        let helpers_code = "helper_func <- function() {}";
        let helpers_tree = parse_r(helpers_code);
        let helpers_artifacts = compute_artifacts(&helpers_uri, &helpers_tree, helpers_code);

        // Build dependency graph with working directory
        let mut graph = DependencyGraph::new();
        let main_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()), // workspace-root-relative
            sources: vec![ForwardSource {
                path: "helpers.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        graph.update_file(&main_uri, &main_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &main_uri { Some(main_artifacts.clone()) }
            else if uri == &helpers_uri { Some(helpers_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &main_uri { Some(main_meta.clone()) }
            else { None }
        };

        // At end of main.R, helper_func should be available because:
        // 1. main.R has @lsp-working-directory /data
        // 2. source("helpers.R") resolves to /project/data/helpers.R
        let scope = scope_at_position_with_graph(
            &main_uri, u32::MAX, u32::MAX, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("helper_func"), "helper_func should be available via working directory directive");
    }

    #[test]
    fn test_function_parameters_available_inside_function() {
        let code = "my_func <- function(x, y) { x + y }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Inside function body, parameters should be available
        let scope_inside = scope_at_position(&artifacts, 0, 30); // Position within function body
        assert!(scope_inside.symbols.contains_key("x"), "Parameter x should be available inside function");
        assert!(scope_inside.symbols.contains_key("y"), "Parameter y should be available inside function");
        assert!(scope_inside.symbols.contains_key("my_func"), "Function name should be available inside function");
    }

    #[test]
    fn test_function_parameters_not_available_outside_function() {
        let code = "my_func <- function(x, y) { x + y }\nresult <- 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside function, parameters should NOT be available
        let scope_outside = scope_at_position(&artifacts, 1, 10); // Position on second line
        assert!(scope_outside.symbols.contains_key("my_func"), "Function name should be available outside function");
        assert!(scope_outside.symbols.contains_key("result"), "Global variable should be available outside function");
        assert!(!scope_outside.symbols.contains_key("x"), "Parameter x should NOT be available outside function");
        assert!(!scope_outside.symbols.contains_key("y"), "Parameter y should NOT be available outside function");
    }

    #[test]
    fn test_function_local_variables_not_available_outside() {
        let code = "my_func <- function() { local_var <- 42; local_var }\nglobal_var <- 100";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside function, local variable should NOT be available
        let scope_outside = scope_at_position(&artifacts, 1, 10);
        assert!(scope_outside.symbols.contains_key("my_func"), "Function name should be available outside function");
        assert!(scope_outside.symbols.contains_key("global_var"), "Global variable should be available outside function");
        assert!(!scope_outside.symbols.contains_key("local_var"), "Function-local variable should NOT be available outside function");

        // Inside function, local variable SHOULD be available
        let scope_inside = scope_at_position(&artifacts, 0, 40);
        assert!(scope_inside.symbols.contains_key("my_func"), "Function name should be available inside function");
        assert!(scope_inside.symbols.contains_key("local_var"), "Function-local variable should be available inside function");
        assert!(!scope_inside.symbols.contains_key("global_var"), "Global variable defined after function should NOT be available inside function");
    }

    #[test]
    fn test_nested_functions_separate_scopes() {
        let code = "outer <- function() { outer_var <- 1; inner <- function() { inner_var <- 2 } }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside all functions
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        assert!(scope_outside.symbols.contains_key("outer"), "Outer function should be available outside");
        assert!(!scope_outside.symbols.contains_key("inner"), "Inner function should NOT be available outside outer function");
        assert!(!scope_outside.symbols.contains_key("outer_var"), "Outer function variable should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("inner_var"), "Inner function variable should NOT be available outside");

        // Inside outer function but outside inner function
        let inner_def_needle = "inner <- function";
        let col_in_outer_after_inner_def = code
            .find(inner_def_needle)
            .or_else(|| code.find("inner"))
            .map(|i| (i + 1) as u32)
            .unwrap_or(0);
        let scope_outer = scope_at_position(&artifacts, 0, col_in_outer_after_inner_def);
        assert!(scope_outer.symbols.contains_key("outer"), "Outer function should be available inside itself");
        assert!(scope_outer.symbols.contains_key("outer_var"), "Outer function variable should be available inside outer function");
        assert!(scope_outer.symbols.contains_key("inner"), "Inner function should be available inside outer function");
        assert!(!scope_outer.symbols.contains_key("inner_var"), "Inner function variable should NOT be available outside inner function");

        // Inside inner function
        let inner_var_def_needle = "inner_var <-";
        let col_in_inner_after_inner_var_def = code
            .rfind(inner_var_def_needle)
            .or_else(|| code.rfind("inner_var"))
            .map(|i| (i + 1) as u32)
            .unwrap_or(0);
        let scope_inner = scope_at_position(&artifacts, 0, col_in_inner_after_inner_var_def);
        assert!(scope_inner.symbols.contains_key("outer"), "Outer function should be available inside inner function");
        assert!(scope_inner.symbols.contains_key("outer_var"), "Outer function variable should be available inside inner function");
        assert!(scope_inner.symbols.contains_key("inner"), "Inner function should be available inside itself");
        assert!(scope_inner.symbols.contains_key("inner_var"), "Inner function variable should be available inside inner function");
    }

    #[test]
    fn test_function_scope_boundaries_with_multiple_functions() {
        let code = "func1 <- function(a) { var1 <- a }\nfunc2 <- function(b) { var2 <- b }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Inside first function
        let scope_func1 = scope_at_position(&artifacts, 0, 25);
        assert!(scope_func1.symbols.contains_key("func1"), "Function 1 should be available inside itself");
        assert!(scope_func1.symbols.contains_key("a"), "Parameter a should be available inside function 1");
        assert!(scope_func1.symbols.contains_key("var1"), "Variable 1 should be available inside function 1");
        assert!(!scope_func1.symbols.contains_key("func2"), "Function 2 should NOT be available inside function 1 (defined later)");
        assert!(!scope_func1.symbols.contains_key("b"), "Parameter b should NOT be available inside function 1");
        assert!(!scope_func1.symbols.contains_key("var2"), "Variable 2 should NOT be available inside function 1");

        // Inside second function
        let scope_func2 = scope_at_position(&artifacts, 1, 25);
        assert!(scope_func2.symbols.contains_key("func1"), "Function 1 should be available inside function 2");
        assert!(scope_func2.symbols.contains_key("func2"), "Function 2 should be available inside itself");
        assert!(scope_func2.symbols.contains_key("b"), "Parameter b should be available inside function 2");
        assert!(scope_func2.symbols.contains_key("var2"), "Variable 2 should be available inside function 2");
        assert!(!scope_func2.symbols.contains_key("a"), "Parameter a should NOT be available inside function 2");
        assert!(!scope_func2.symbols.contains_key("var1"), "Variable 1 should NOT be available inside function 2");

        // Outside both functions
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        assert!(scope_outside.symbols.contains_key("func1"), "Function 1 should be available outside");
        assert!(scope_outside.symbols.contains_key("func2"), "Function 2 should be available outside");
        assert!(!scope_outside.symbols.contains_key("a"), "Parameter a should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("b"), "Parameter b should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("var1"), "Variable 1 should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("var2"), "Variable 2 should NOT be available outside");
    }

    #[test]
    fn test_function_with_default_parameter_values() {
        let code = "my_func <- function(x = 1, y = 2) { x * y }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have FunctionScope event with parameters
        let function_scope_event = artifacts.timeline.iter().find(|event| {
            matches!(event, ScopeEvent::FunctionScope { .. })
        });
        assert!(function_scope_event.is_some());

        if let Some(ScopeEvent::FunctionScope { parameters, .. }) = function_scope_event {
            assert_eq!(parameters.len(), 2);
            let param_names: Vec<&str> = parameters.iter().map(|p| p.name.as_str()).collect();
            assert!(param_names.contains(&"x"));
            assert!(param_names.contains(&"y"));
        }

        // Parameters should be available within function body
        let scope_in_body = scope_at_position(&artifacts, 0, 40);
        assert!(scope_in_body.symbols.contains_key("x"));
        assert!(scope_in_body.symbols.contains_key("y"));
    }

    #[test]
    fn test_function_with_ellipsis_parameter() {
        let code = "my_func <- function(x, ...) { list(x, ...) }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have FunctionScope event with parameters including ellipsis
        let function_scope_event = artifacts.timeline.iter().find(|event| {
            matches!(event, ScopeEvent::FunctionScope { .. })
        });
        assert!(function_scope_event.is_some());

        if let Some(ScopeEvent::FunctionScope { parameters, .. }) = function_scope_event {
            assert_eq!(parameters.len(), 2);
            let param_names: Vec<&str> = parameters.iter().map(|p| p.name.as_str()).collect();
            assert!(param_names.contains(&"x"));
            assert!(param_names.contains(&"..."));
        }

        // Parameters should be available within function body
        let scope_in_body = scope_at_position(&artifacts, 0, 40);
        assert!(scope_in_body.symbols.contains_key("x"));
        assert!(scope_in_body.symbols.contains_key("..."));
    }

    #[test]
    fn test_function_with_no_parameters() {
        let code = "my_func <- function() { 42 }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have FunctionScope event with empty parameters
        let function_scope_event = artifacts.timeline.iter().find(|event| {
            matches!(event, ScopeEvent::FunctionScope { .. })
        });
        assert!(function_scope_event.is_some());

        if let Some(ScopeEvent::FunctionScope { parameters, .. }) = function_scope_event {
            assert_eq!(parameters.len(), 0);
        }

        // Function name should still be available within body
        let scope_in_body = scope_at_position(&artifacts, 0, 25);
        assert!(scope_in_body.symbols.contains_key("my_func"));
    }

    #[test]
    fn test_eof_position_does_not_match_all_functions() {
        // Test that querying at EOF (u32::MAX) doesn't incorrectly include function parameters
        let code = "func1 <- function(param1) { var1 <- 1 }\nfunc2 <- function(param2) { var2 <- 2 }\nglobal_var <- 3";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Query at EOF position
        let scope_eof = scope_at_position(&artifacts, u32::MAX, u32::MAX);
        
        // Should have global symbols
        assert!(scope_eof.symbols.contains_key("func1"), "func1 should be available at EOF");
        assert!(scope_eof.symbols.contains_key("func2"), "func2 should be available at EOF");
        assert!(scope_eof.symbols.contains_key("global_var"), "global_var should be available at EOF");
        
        // Should NOT have function parameters (this was the bug)
        assert!(!scope_eof.symbols.contains_key("param1"), "param1 should NOT be available at EOF");
        assert!(!scope_eof.symbols.contains_key("param2"), "param2 should NOT be available at EOF");
        
        // Should NOT have function-local variables
        assert!(!scope_eof.symbols.contains_key("var1"), "var1 should NOT be available at EOF");
        assert!(!scope_eof.symbols.contains_key("var2"), "var2 should NOT be available at EOF");
    }

    // ============================================================================
    // Tests for ScopeEvent::Removal (Task 1.2)
    // Validates: Requirements 1.1, 1.2
    // ============================================================================

    #[test]
    fn test_removal_event_creation_single_symbol() {
        // Test that Removal events can be created with line, column, and a single symbol
        let removal = ScopeEvent::Removal {
            line: 5,
            column: 0,
            symbols: vec!["x".to_string()],
            function_scope: None,
        };

        match removal {
            ScopeEvent::Removal { line, column, symbols, .. } => {
                assert_eq!(line, 5);
                assert_eq!(column, 0);
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0], "x");
            }
            _ => panic!("Expected Removal event"),
        }
    }

    #[test]
    fn test_removal_event_creation_multiple_symbols() {
        // Test that Removal events can be created with multiple symbols
        let removal = ScopeEvent::Removal {
            line: 10,
            column: 4,
            symbols: vec!["x".to_string(), "y".to_string(), "z".to_string()],
            function_scope: None,
        };

        match removal {
            ScopeEvent::Removal { line, column, symbols, .. } => {
                assert_eq!(line, 10);
                assert_eq!(column, 4);
                assert_eq!(symbols.len(), 3);
                assert!(symbols.contains(&"x".to_string()));
                assert!(symbols.contains(&"y".to_string()));
                assert!(symbols.contains(&"z".to_string()));
            }
            _ => panic!("Expected Removal event"),
        }
    }

    #[test]
    fn test_removal_event_creation_empty_symbols() {
        // Test that Removal events can be created with empty symbols list (edge case)
        let removal = ScopeEvent::Removal {
            line: 0,
            column: 0,
            symbols: vec![],
            function_scope: None,
        };

        match removal {
            ScopeEvent::Removal { line, column, symbols, .. } => {
                assert_eq!(line, 0);
                assert_eq!(column, 0);
                assert!(symbols.is_empty());
            }
            _ => panic!("Expected Removal event"),
        }
    }

    #[test]
    fn test_removal_event_sorting_by_position() {
        // Test that Removal events are correctly sorted by (line, column) position
        let mut events = vec![
            ScopeEvent::Removal {
                line: 5,
                column: 10,
                symbols: vec!["c".to_string()],
                function_scope: None,
            },
            ScopeEvent::Removal {
                line: 2,
                column: 0,
                symbols: vec!["a".to_string()],
                function_scope: None,
            },
            ScopeEvent::Removal {
                line: 5,
                column: 5,
                symbols: vec!["b".to_string()],
                function_scope: None,
            },
            ScopeEvent::Removal {
                line: 10,
                column: 0,
                symbols: vec!["d".to_string()],
                function_scope: None,
            },
        ];

        // Sort using the same key as compute_artifacts
        events.sort_by_key(|event| match event {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Verify order: (2,0), (5,5), (5,10), (10,0)
        let positions: Vec<(u32, u32)> = events.iter().map(|e| match e {
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            _ => panic!("Expected Removal event"),
        }).collect();

        assert_eq!(positions, vec![(2, 0), (5, 5), (5, 10), (10, 0)]);
    }

    #[test]
    fn test_removal_event_sorting_same_line_different_columns() {
        // Test that Removal events on the same line are sorted by column
        let mut events = vec![
            ScopeEvent::Removal {
                line: 3,
                column: 20,
                symbols: vec!["c".to_string()],
                function_scope: None,
            },
            ScopeEvent::Removal {
                line: 3,
                column: 5,
                symbols: vec!["a".to_string()],
                function_scope: None,
            },
            ScopeEvent::Removal {
                line: 3,
                column: 10,
                symbols: vec!["b".to_string()],
                function_scope: None,
            },
        ];

        events.sort_by_key(|event| match event {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Verify order by column: 5, 10, 20
        let columns: Vec<u32> = events.iter().map(|e| match e {
            ScopeEvent::Removal { column, .. } => *column,
            _ => panic!("Expected Removal event"),
        }).collect();

        assert_eq!(columns, vec![5, 10, 20]);
    }

    #[test]
    fn test_removal_event_mixed_with_def_events() {
        // Test that Removal events sort correctly when mixed with Def events
        let uri = test_uri();
        let mut events = vec![
            ScopeEvent::Removal {
                line: 3,
                column: 0,
                symbols: vec!["x".to_string()],
                function_scope: None,
            },
            ScopeEvent::Def {
                line: 1,
                column: 0,
                symbol: ScopedSymbol {
                    name: "x".to_string(),
                    kind: SymbolKind::Variable,
                    source_uri: uri.clone(),
                    defined_line: 1,
                    defined_column: 0,
                    signature: None,
                },
            },
            ScopeEvent::Def {
                line: 5,
                column: 0,
                symbol: ScopedSymbol {
                    name: "y".to_string(),
                    kind: SymbolKind::Variable,
                    source_uri: uri.clone(),
                    defined_line: 5,
                    defined_column: 0,
                    signature: None,
                },
            },
        ];

        events.sort_by_key(|event| match event {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Verify order: Def(1,0), Removal(3,0), Def(5,0)
        let event_types: Vec<&str> = events.iter().map(|e| match e {
            ScopeEvent::Def { .. } => "Def",
            ScopeEvent::Removal { .. } => "Removal",
            _ => "Other",
        }).collect();

        assert_eq!(event_types, vec!["Def", "Removal", "Def"]);

        // Verify positions
        let positions: Vec<(u32, u32)> = events.iter().map(|e| match e {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            _ => (0, 0),
        }).collect();

        assert_eq!(positions, vec![(1, 0), (3, 0), (5, 0)]);
    }

    #[test]
    fn test_removal_event_mixed_with_source_events() {
        // Test that Removal events sort correctly when mixed with Source events
        use super::super::types::ForwardSource;

        let mut events = vec![
            ScopeEvent::Removal {
                line: 2,
                column: 0,
                symbols: vec!["x".to_string()],
                function_scope: None,
            },
            ScopeEvent::Source {
                line: 1,
                column: 0,
                source: ForwardSource {
                    path: "child.R".to_string(),
                    line: 1,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            },
            ScopeEvent::Removal {
                line: 4,
                column: 0,
                symbols: vec!["y".to_string()],
                function_scope: None,
            },
        ];

        events.sort_by_key(|event| match event {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Verify order: Source(1,0), Removal(2,0), Removal(4,0)
        let event_types: Vec<&str> = events.iter().map(|e| match e {
            ScopeEvent::Source { .. } => "Source",
            ScopeEvent::Removal { .. } => "Removal",
            _ => "Other",
        }).collect();

        assert_eq!(event_types, vec!["Source", "Removal", "Removal"]);
    }

    #[test]
    fn test_removal_event_mixed_with_all_event_types() {
        // Test that Removal events sort correctly when mixed with Def, Source, and FunctionScope events
        use super::super::types::ForwardSource;

        let uri = test_uri();
        let mut events = vec![
            ScopeEvent::Removal {
                line: 5,
                column: 0,
                symbols: vec!["z".to_string()],
                function_scope: None,
            },
            ScopeEvent::Def {
                line: 1,
                column: 0,
                symbol: ScopedSymbol {
                    name: "x".to_string(),
                    kind: SymbolKind::Variable,
                    source_uri: uri.clone(),
                    defined_line: 1,
                    defined_column: 0,
                    signature: None,
                },
            },
            ScopeEvent::Source {
                line: 3,
                column: 0,
                source: ForwardSource {
                    path: "child.R".to_string(),
                    line: 3,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            },
            ScopeEvent::FunctionScope {
                start_line: 7,
                start_column: 0,
                end_line: 10,
                end_column: 1,
                parameters: vec![],
            },
            ScopeEvent::Removal {
                line: 9,
                column: 0,
                symbols: vec!["w".to_string()],
                function_scope: None,
            },
        ];

        events.sort_by_key(|event| match event {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Verify order: Def(1,0), Source(3,0), Removal(5,0), FunctionScope(7,0), Removal(9,0)
        let event_types: Vec<&str> = events.iter().map(|e| match e {
            ScopeEvent::Def { .. } => "Def",
            ScopeEvent::Source { .. } => "Source",
            ScopeEvent::FunctionScope { .. } => "FunctionScope",
            ScopeEvent::Removal { .. } => "Removal",
            ScopeEvent::PackageLoad { .. } => "PackageLoad",
        }).collect();

        assert_eq!(event_types, vec!["Def", "Source", "Removal", "FunctionScope", "Removal"]);

        // Verify positions
        let positions: Vec<u32> = events.iter().map(|e| match e {
            ScopeEvent::Def { line, .. } => *line,
            ScopeEvent::Source { line, .. } => *line,
            ScopeEvent::FunctionScope { start_line, .. } => *start_line,
            ScopeEvent::Removal { line, .. } => *line,
            ScopeEvent::PackageLoad { line, .. } => *line,
        }).collect();

        assert_eq!(positions, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn test_removal_event_at_same_position_as_def() {
        // Test sorting when Removal and Def events are at the same position
        // (This is an edge case - in practice they would be at different positions)
        let uri = test_uri();
        let mut events = vec![
            ScopeEvent::Removal {
                line: 2,
                column: 0,
                symbols: vec!["x".to_string()],
                function_scope: None,
            },
            ScopeEvent::Def {
                line: 2,
                column: 0,
                symbol: ScopedSymbol {
                    name: "y".to_string(),
                    kind: SymbolKind::Variable,
                    source_uri: uri.clone(),
                    defined_line: 2,
                    defined_column: 0,
                    signature: None,
                },
            },
        ];

        events.sort_by_key(|event| match event {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
            ScopeEvent::Removal { line, column, .. } => (*line, *column),
            ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
        });

        // Both events should be at position (2, 0) - order between them is stable but not guaranteed
        // The important thing is that both are present and at the same position
        assert_eq!(events.len(), 2);
        for event in &events {
            let pos = match event {
                ScopeEvent::Def { line, column, .. } => (*line, *column),
                ScopeEvent::Removal { line, column, .. } => (*line, *column),
                _ => panic!("Unexpected event type"),
            };
            assert_eq!(pos, (2, 0));
        }
    }

    #[test]
    fn test_removal_event_clone() {
        // Test that Removal events can be cloned (derives Clone)
        let original = ScopeEvent::Removal {
            line: 5,
            column: 10,
            symbols: vec!["x".to_string(), "y".to_string()],
            function_scope: None,
        };

        let cloned = original.clone();

        match (original, cloned) {
            (
                ScopeEvent::Removal { line: l1, column: c1, symbols: s1, .. },
                ScopeEvent::Removal { line: l2, column: c2, symbols: s2, .. },
            ) => {
                assert_eq!(l1, l2);
                assert_eq!(c1, c2);
                assert_eq!(s1, s2);
            }
            _ => panic!("Expected Removal events"),
        }
    }

    // ============================================================================
    // Integration tests for artifacts with removals (Task 4.2)
    // Validates: Requirements 1.1, 7.1
    // ============================================================================

    #[test]
    fn test_artifacts_define_then_remove() {
        // Test: x <- 1; rm(x) - timeline should have Def then Removal
        // Validates: Requirements 1.1, 7.1
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have both Def and Removal events in timeline
        let def_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, symbol, .. } => Some((*line, symbol.name.clone())),
                _ => None,
            })
            .collect();

        let removal_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Removal { line, symbols, .. } => Some((*line, symbols.clone())),
                _ => None,
            })
            .collect();

        // Verify Def event for x on line 0
        assert_eq!(def_events.len(), 1, "Should have one Def event");
        assert_eq!(def_events[0].0, 0, "Def should be on line 0");
        assert_eq!(def_events[0].1, "x", "Def should be for symbol 'x'");

        // Verify Removal event for x on line 1
        assert_eq!(removal_events.len(), 1, "Should have one Removal event");
        assert_eq!(removal_events[0].0, 1, "Removal should be on line 1");
        assert!(removal_events[0].1.contains(&"x".to_string()), "Removal should contain 'x'");

        // Verify timeline order: Def comes before Removal
        let timeline_order: Vec<(&str, u32)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, .. } => Some(("Def", *line)),
                ScopeEvent::Removal { line, .. } => Some(("Removal", *line)),
                _ => None,
            })
            .collect();

        assert_eq!(timeline_order.len(), 2);
        assert_eq!(timeline_order[0], ("Def", 0), "Def should come first");
        assert_eq!(timeline_order[1], ("Removal", 1), "Removal should come second");
    }

    #[test]
    fn test_artifacts_remove_then_define() {
        // Test: rm(x); x <- 1 - timeline should have Removal then Def
        // Validates: Requirements 1.1, 7.1
        let code = "rm(x)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Verify timeline order: Removal comes before Def
        let timeline_order: Vec<(&str, u32)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, .. } => Some(("Def", *line)),
                ScopeEvent::Removal { line, .. } => Some(("Removal", *line)),
                _ => None,
            })
            .collect();

        assert_eq!(timeline_order.len(), 2);
        assert_eq!(timeline_order[0], ("Removal", 0), "Removal should come first");
        assert_eq!(timeline_order[1], ("Def", 1), "Def should come second");
    }

    #[test]
    fn test_artifacts_multiple_definitions_and_removals() {
        // Test: x <- 1; y <- 2; rm(x); z <- 3; rm(y, z)
        // Timeline should have: Def(x), Def(y), Removal(x), Def(z), Removal(y,z)
        // Validates: Requirements 1.1, 1.2, 7.1
        let code = "x <- 1\ny <- 2\nrm(x)\nz <- 3\nrm(y, z)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Collect all events with their types and positions
        let timeline_events: Vec<(&str, u32, Vec<String>)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, symbol, .. } => Some(("Def", *line, vec![symbol.name.clone()])),
                ScopeEvent::Removal { line, symbols, .. } => Some(("Removal", *line, symbols.clone())),
                _ => None,
            })
            .collect();

        // Should have 5 events total: 3 Defs and 2 Removals
        assert_eq!(timeline_events.len(), 5, "Should have 5 events (3 Defs + 2 Removals)");

        // Verify order and content
        assert_eq!(timeline_events[0], ("Def", 0, vec!["x".to_string()]), "First: Def x on line 0");
        assert_eq!(timeline_events[1], ("Def", 1, vec!["y".to_string()]), "Second: Def y on line 1");
        assert_eq!(timeline_events[2].0, "Removal", "Third: Removal");
        assert_eq!(timeline_events[2].1, 2, "Third: on line 2");
        assert!(timeline_events[2].2.contains(&"x".to_string()), "Third: contains x");
        assert_eq!(timeline_events[3], ("Def", 3, vec!["z".to_string()]), "Fourth: Def z on line 3");
        assert_eq!(timeline_events[4].0, "Removal", "Fifth: Removal");
        assert_eq!(timeline_events[4].1, 4, "Fifth: on line 4");
        assert!(timeline_events[4].2.contains(&"y".to_string()), "Fifth: contains y");
        assert!(timeline_events[4].2.contains(&"z".to_string()), "Fifth: contains z");
    }

    #[test]
    fn test_artifacts_removal_with_source() {
        // Test: source("utils.R"); rm(helper_func)
        // Timeline should have: Source, Removal
        // Validates: Requirements 1.1, 7.1
        let code = "source(\"utils.R\")\nrm(helper_func)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Collect all events with their types and positions
        let timeline_events: Vec<(&str, u32)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { line, .. } => Some(("Source", *line)),
                ScopeEvent::Removal { line, .. } => Some(("Removal", *line)),
                _ => None,
            })
            .collect();

        // Should have 2 events: Source and Removal
        assert_eq!(timeline_events.len(), 2, "Should have 2 events (Source + Removal)");

        // Verify order
        assert_eq!(timeline_events[0], ("Source", 0), "First: Source on line 0");
        assert_eq!(timeline_events[1], ("Removal", 1), "Second: Removal on line 1");

        // Verify the removal contains the correct symbol
        let removal_symbols: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Removal { symbols, .. } => Some(symbols.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(removal_symbols.len(), 1);
        assert!(removal_symbols[0].contains(&"helper_func".to_string()), 
            "Removal should contain 'helper_func'");
    }

    #[test]
    fn test_artifacts_removal_with_remove_alias() {
        // Test: x <- 1; remove(x) - using remove() alias
        // Validates: Requirements 2.1, 2.2
        let code = "x <- 1\nremove(x)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Verify timeline has both Def and Removal
        let timeline_order: Vec<(&str, u32)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, .. } => Some(("Def", *line)),
                ScopeEvent::Removal { line, .. } => Some(("Removal", *line)),
                _ => None,
            })
            .collect();

        assert_eq!(timeline_order.len(), 2);
        assert_eq!(timeline_order[0], ("Def", 0), "Def should come first");
        assert_eq!(timeline_order[1], ("Removal", 1), "Removal should come second");

        // Verify the removal contains the correct symbol
        let removal_symbols: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Removal { symbols, .. } => Some(symbols.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(removal_symbols.len(), 1);
        assert!(removal_symbols[0].contains(&"x".to_string()), 
            "Removal via remove() should contain 'x'");
    }

    #[test]
    fn test_artifacts_removal_with_list_argument() {
        // Test: x <- 1; y <- 2; rm(list = c("x", "y"))
        // Validates: Requirements 3.1, 3.2
        let code = "x <- 1\ny <- 2\nrm(list = c(\"x\", \"y\"))";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Verify timeline has Defs and Removal
        let timeline_events: Vec<(&str, u32)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, .. } => Some(("Def", *line)),
                ScopeEvent::Removal { line, .. } => Some(("Removal", *line)),
                _ => None,
            })
            .collect();

        assert_eq!(timeline_events.len(), 3, "Should have 3 events (2 Defs + 1 Removal)");
        assert_eq!(timeline_events[0], ("Def", 0));
        assert_eq!(timeline_events[1], ("Def", 1));
        assert_eq!(timeline_events[2], ("Removal", 2));

        // Verify the removal contains both symbols
        let removal_symbols: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Removal { symbols, .. } => Some(symbols.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(removal_symbols.len(), 1);
        assert!(removal_symbols[0].contains(&"x".to_string()), 
            "Removal should contain 'x'");
        assert!(removal_symbols[0].contains(&"y".to_string()), 
            "Removal should contain 'y'");
    }

    #[test]
    fn test_artifacts_removal_mixed_bare_and_list() {
        // Test: rm(a, list = c("b", "c"))
        // Validates: Requirements 1.1, 3.1, 3.2
        let code = "a <- 1\nb <- 2\nc <- 3\nrm(a, list = c(\"b\", \"c\"))";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Verify the removal contains all three symbols
        let removal_symbols: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Removal { symbols, .. } => Some(symbols.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(removal_symbols.len(), 1, "Should have one Removal event");
        assert!(removal_symbols[0].contains(&"a".to_string()), 
            "Removal should contain 'a' (bare symbol)");
        assert!(removal_symbols[0].contains(&"b".to_string()), 
            "Removal should contain 'b' (from list)");
        assert!(removal_symbols[0].contains(&"c".to_string()), 
            "Removal should contain 'c' (from list)");
    }

    #[test]
    fn test_artifacts_removal_with_function_scope() {
        // Test: rm() inside a function should still be in timeline
        // Validates: Requirements 1.1, 5.1
        let code = "my_func <- function() {\n  x <- 1\n  rm(x)\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Verify timeline has FunctionScope, Def, and Removal
        let has_function_scope = artifacts.timeline.iter().any(|e| matches!(e, ScopeEvent::FunctionScope { .. }));
        let has_removal = artifacts.timeline.iter().any(|e| matches!(e, ScopeEvent::Removal { .. }));

        assert!(has_function_scope, "Should have FunctionScope event");
        assert!(has_removal, "Should have Removal event inside function");

        // Verify the removal is for 'x'
        let removal_symbols: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Removal { symbols, .. } => Some(symbols.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(removal_symbols.len(), 1);
        assert!(removal_symbols[0].contains(&"x".to_string()), 
            "Removal should contain 'x'");
    }

    #[test]
    fn test_artifacts_no_removal_for_envir_argument() {
        // Test: rm(x, envir = my_env) should NOT create a Removal event
        // Validates: Requirements 4.1
        let code = "x <- 1\nrm(x, envir = my_env)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have Def but no Removal (envir= filters it out)
        let removal_count = artifacts.timeline.iter()
            .filter(|e| matches!(e, ScopeEvent::Removal { .. }))
            .count();

        assert_eq!(removal_count, 0, "Should have no Removal events when envir= is non-default");
    }

    #[test]
    fn test_artifacts_removal_with_globalenv() {
        // Test: rm(x, envir = globalenv()) SHOULD create a Removal event
        // Validates: Requirements 4.3
        let code = "x <- 1\nrm(x, envir = globalenv())";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have both Def and Removal (globalenv() is default-equivalent)
        let removal_count = artifacts.timeline.iter()
            .filter(|e| matches!(e, ScopeEvent::Removal { .. }))
            .count();

        assert_eq!(removal_count, 1, "Should have one Removal event when envir=globalenv()");
    }

    #[test]
    fn test_artifacts_timeline_sorting_with_removals() {
        // Test that timeline is correctly sorted when mixing Def, Source, Removal, and FunctionScope
        // Validates: Requirements 1.1, 7.1
        let code = "a <- 1\nsource(\"utils.R\")\nb <- 2\nrm(a)\nc <- 3";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Collect all events with their types and line numbers
        let timeline_events: Vec<(&str, u32)> = artifacts.timeline.iter()
            .map(|e| match e {
                ScopeEvent::Def { line, .. } => ("Def", *line),
                ScopeEvent::Source { line, .. } => ("Source", *line),
                ScopeEvent::FunctionScope { start_line, .. } => ("FunctionScope", *start_line),
                ScopeEvent::Removal { line, .. } => ("Removal", *line),
                ScopeEvent::PackageLoad { line, .. } => ("PackageLoad", *line),
            })
            .collect();

        // Verify events are sorted by line number
        let lines: Vec<u32> = timeline_events.iter().map(|(_, line)| *line).collect();
        let mut sorted_lines = lines.clone();
        sorted_lines.sort();
        assert_eq!(lines, sorted_lines, "Timeline should be sorted by line number");

        // Verify expected order: Def(0), Source(1), Def(2), Removal(3), Def(4)
        assert_eq!(timeline_events[0], ("Def", 0), "First: Def a on line 0");
        assert_eq!(timeline_events[1], ("Source", 1), "Second: Source on line 1");
        assert_eq!(timeline_events[2], ("Def", 2), "Third: Def b on line 2");
        assert_eq!(timeline_events[3], ("Removal", 3), "Fourth: Removal on line 3");
        assert_eq!(timeline_events[4], ("Def", 4), "Fifth: Def c on line 4");
    }

    // ============================================================================
    // Unit tests for scope resolution with removals (Task 5.4)
    // Validates: Requirements 7.1, 7.2, 7.3, 7.4
    // ============================================================================

    #[test]
    fn test_scope_define_then_remove() {
        // Test: x <- 1; rm(x) - x should NOT be in scope after rm()
        // Validates: Requirements 7.3, 7.4
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Before rm() (line 0, after definition), x should be in scope
        let scope_before_rm = scope_at_position(&artifacts, 0, 10);
        assert!(scope_before_rm.symbols.contains_key("x"), 
            "x should be in scope after definition but before rm()");

        // After rm() (line 1, after rm call), x should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after rm()");

        // At end of file, x should NOT be in scope
        let scope_eof = scope_at_position(&artifacts, 10, 0);
        assert!(!scope_eof.symbols.contains_key("x"), 
            "x should NOT be in scope at end of file after rm()");
    }

    #[test]
    fn test_scope_remove_then_define() {
        // Test: rm(x); x <- 1 - x should be in scope after definition
        // Validates: Requirements 7.2
        let code = "rm(x)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // After rm() but before definition (line 0), x should NOT be in scope
        // (rm() on undefined symbol has no effect, but x is still not defined)
        let scope_after_rm = scope_at_position(&artifacts, 0, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after rm() of undefined symbol");

        // After definition (line 1), x should be in scope
        let scope_after_def = scope_at_position(&artifacts, 1, 10);
        assert!(scope_after_def.symbols.contains_key("x"), 
            "x should be in scope after definition");

        // At end of file, x should be in scope
        let scope_eof = scope_at_position(&artifacts, 10, 0);
        assert!(scope_eof.symbols.contains_key("x"), 
            "x should be in scope at end of file after definition");
    }

    #[test]
    fn test_scope_define_remove_define() {
        // Test: x <- 1; rm(x); x <- 2 - x should be in scope after second definition
        // Validates: Requirements 7.1
        let code = "x <- 1\nrm(x)\nx <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // After first definition (line 0), x should be in scope
        let scope_after_first_def = scope_at_position(&artifacts, 0, 10);
        assert!(scope_after_first_def.symbols.contains_key("x"), 
            "x should be in scope after first definition");

        // After rm() (line 1), x should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after rm()");

        // After second definition (line 2), x should be in scope again
        let scope_after_second_def = scope_at_position(&artifacts, 2, 10);
        assert!(scope_after_second_def.symbols.contains_key("x"), 
            "x should be in scope after second definition");

        // At end of file, x should be in scope
        let scope_eof = scope_at_position(&artifacts, 10, 0);
        assert!(scope_eof.symbols.contains_key("x"), 
            "x should be in scope at end of file after re-definition");
    }

    #[test]
    fn test_scope_position_aware_queries() {
        // Test position-aware queries at different points in the code
        // Validates: Requirements 7.3, 7.4
        let code = "a <- 1\nb <- 2\nrm(a)\nc <- 3\nrm(b, c)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Line 0: only 'a' is defined
        let scope_line0 = scope_at_position(&artifacts, 0, 10);
        assert!(scope_line0.symbols.contains_key("a"), "a should be in scope on line 0");
        assert!(!scope_line0.symbols.contains_key("b"), "b should NOT be in scope on line 0");
        assert!(!scope_line0.symbols.contains_key("c"), "c should NOT be in scope on line 0");

        // Line 1: 'a' and 'b' are defined
        let scope_line1 = scope_at_position(&artifacts, 1, 10);
        assert!(scope_line1.symbols.contains_key("a"), "a should be in scope on line 1");
        assert!(scope_line1.symbols.contains_key("b"), "b should be in scope on line 1");
        assert!(!scope_line1.symbols.contains_key("c"), "c should NOT be in scope on line 1");

        // Line 2: 'a' is removed, only 'b' remains
        let scope_line2 = scope_at_position(&artifacts, 2, 10);
        assert!(!scope_line2.symbols.contains_key("a"), "a should NOT be in scope on line 2 (after rm)");
        assert!(scope_line2.symbols.contains_key("b"), "b should be in scope on line 2");
        assert!(!scope_line2.symbols.contains_key("c"), "c should NOT be in scope on line 2");

        // Line 3: 'b' and 'c' are defined, 'a' is still removed
        let scope_line3 = scope_at_position(&artifacts, 3, 10);
        assert!(!scope_line3.symbols.contains_key("a"), "a should NOT be in scope on line 3");
        assert!(scope_line3.symbols.contains_key("b"), "b should be in scope on line 3");
        assert!(scope_line3.symbols.contains_key("c"), "c should be in scope on line 3");

        // Line 4: 'b' and 'c' are removed, nothing remains
        let scope_line4 = scope_at_position(&artifacts, 4, 10);
        assert!(!scope_line4.symbols.contains_key("a"), "a should NOT be in scope on line 4");
        assert!(!scope_line4.symbols.contains_key("b"), "b should NOT be in scope on line 4 (after rm)");
        assert!(!scope_line4.symbols.contains_key("c"), "c should NOT be in scope on line 4 (after rm)");
    }

    #[test]
    fn test_scope_removal_multiple_symbols() {
        // Test: x <- 1; y <- 2; rm(x, y) - both should be removed
        // Validates: Requirements 1.2, 7.4
        let code = "x <- 1\ny <- 2\nrm(x, y)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Before rm() (line 1), both x and y should be in scope
        let scope_before_rm = scope_at_position(&artifacts, 1, 10);
        assert!(scope_before_rm.symbols.contains_key("x"), "x should be in scope before rm()");
        assert!(scope_before_rm.symbols.contains_key("y"), "y should be in scope before rm()");

        // After rm() (line 2), neither x nor y should be in scope
        let scope_after_rm = scope_at_position(&artifacts, 2, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), "x should NOT be in scope after rm()");
        assert!(!scope_after_rm.symbols.contains_key("y"), "y should NOT be in scope after rm()");
    }

    #[test]
    fn test_scope_removal_with_list_argument() {
        // Test: x <- 1; y <- 2; rm(list = c("x", "y")) - both should be removed
        // Validates: Requirements 3.1, 3.2, 7.4
        let code = "x <- 1\ny <- 2\nrm(list = c(\"x\", \"y\"))";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Before rm() (line 1), both x and y should be in scope
        let scope_before_rm = scope_at_position(&artifacts, 1, 10);
        assert!(scope_before_rm.symbols.contains_key("x"), "x should be in scope before rm()");
        assert!(scope_before_rm.symbols.contains_key("y"), "y should be in scope before rm()");

        // After rm() (line 2), neither x nor y should be in scope
        let scope_after_rm = scope_at_position(&artifacts, 2, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), "x should NOT be in scope after rm(list=...)");
        assert!(!scope_after_rm.symbols.contains_key("y"), "y should NOT be in scope after rm(list=...)");
    }

    #[test]
    fn test_scope_removal_using_remove_alias() {
        // Test: x <- 1; remove(x) - x should NOT be in scope after remove()
        // Validates: Requirements 2.1, 2.2, 7.4
        let code = "x <- 1\nremove(x)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Before remove() (line 0), x should be in scope
        let scope_before_rm = scope_at_position(&artifacts, 0, 10);
        assert!(scope_before_rm.symbols.contains_key("x"), 
            "x should be in scope before remove()");

        // After remove() (line 1), x should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after remove()");
    }

    #[test]
    fn test_scope_removal_does_not_affect_other_symbols() {
        // Test: x <- 1; y <- 2; rm(x) - y should still be in scope
        // Validates: Requirements 7.3, 7.4
        let code = "x <- 1\ny <- 2\nrm(x)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // After rm(x) (line 2), x should NOT be in scope but y should be
        let scope_after_rm = scope_at_position(&artifacts, 2, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after rm(x)");
        assert!(scope_after_rm.symbols.contains_key("y"), 
            "y should still be in scope after rm(x)");
    }

    #[test]
    fn test_scope_removal_inside_function_local_only() {
        // Test: rm() inside a function should only affect that function's scope
        // Validates: Requirements 5.1, 5.2, 5.3
        let code = "x <- 1\nmy_func <- function() {\n  y <- 2\n  rm(y)\n  z <- 3\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside function (after function definition), x should be in scope
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        assert!(scope_outside.symbols.contains_key("x"), 
            "x should be in scope outside function");
        assert!(scope_outside.symbols.contains_key("my_func"), 
            "my_func should be in scope outside function");
        // y and z are function-local, should NOT be in global scope
        assert!(!scope_outside.symbols.contains_key("y"), 
            "y should NOT be in global scope (function-local)");
        assert!(!scope_outside.symbols.contains_key("z"), 
            "z should NOT be in global scope (function-local)");

        // Inside function, after rm(y) but before z definition (line 3)
        // Find position inside function body after rm(y)
        let scope_inside_after_rm = scope_at_position(&artifacts, 3, 10);
        assert!(!scope_inside_after_rm.symbols.contains_key("y"), 
            "y should NOT be in scope inside function after rm(y)");

        // Inside function, after z definition (line 4)
        let scope_inside_after_z = scope_at_position(&artifacts, 4, 10);
        assert!(scope_inside_after_z.symbols.contains_key("z"), 
            "z should be in scope inside function after definition");
        assert!(!scope_inside_after_z.symbols.contains_key("y"), 
            "y should still NOT be in scope after rm(y)");
    }

    #[test]
    fn test_scope_global_removal_does_not_affect_function_scope() {
        // Test: Global rm() should not affect symbols inside functions
        // Validates: Requirements 5.1, 5.2
        let code = "x <- 1\nrm(x)\nmy_func <- function() {\n  y <- 2\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // After rm(x) at global level, x should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after global rm(x)");

        // Inside function, y should be in scope (unaffected by global rm)
        let scope_inside_func = scope_at_position(&artifacts, 3, 10);
        assert!(scope_inside_func.symbols.contains_key("y"), 
            "y should be in scope inside function");
        assert!(!scope_inside_func.symbols.contains_key("x"), 
            "x should NOT be in scope inside function (removed globally before function)");
    }

    #[test]
    fn test_scope_removal_with_envir_globalenv() {
        // Test: rm(x, envir = globalenv()) should still remove x
        // Validates: Requirements 4.2, 4.3
        let code = "x <- 1\nrm(x, envir = globalenv())";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // After rm() with envir=globalenv(), x should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 10);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after rm(x, envir=globalenv())");
    }

    #[test]
    fn test_scope_removal_with_envir_non_default_ignored() {
        // Test: rm(x, envir = my_env) should NOT remove x from scope
        // Validates: Requirements 4.1
        let code = "x <- 1\nrm(x, envir = my_env)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // After rm() with non-default envir, x should still be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 10);
        assert!(scope_after_rm.symbols.contains_key("x"), 
            "x should still be in scope after rm(x, envir=my_env) - non-default envir is ignored");
    }

    #[test]
    fn test_scope_removal_complex_sequence() {
        // Test a complex sequence of definitions and removals
        // Validates: Requirements 7.1, 7.2, 7.3, 7.4
        let code = "a <- 1\nb <- 2\nrm(a)\na <- 3\nc <- 4\nrm(b, c)\na <- 5";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Line 0: a defined
        let scope_l0 = scope_at_position(&artifacts, 0, 10);
        assert!(scope_l0.symbols.contains_key("a"));
        assert!(!scope_l0.symbols.contains_key("b"));
        assert!(!scope_l0.symbols.contains_key("c"));

        // Line 1: a, b defined
        let scope_l1 = scope_at_position(&artifacts, 1, 10);
        assert!(scope_l1.symbols.contains_key("a"));
        assert!(scope_l1.symbols.contains_key("b"));
        assert!(!scope_l1.symbols.contains_key("c"));

        // Line 2: a removed, b remains
        let scope_l2 = scope_at_position(&artifacts, 2, 10);
        assert!(!scope_l2.symbols.contains_key("a"));
        assert!(scope_l2.symbols.contains_key("b"));
        assert!(!scope_l2.symbols.contains_key("c"));

        // Line 3: a re-defined, b remains
        let scope_l3 = scope_at_position(&artifacts, 3, 10);
        assert!(scope_l3.symbols.contains_key("a"));
        assert!(scope_l3.symbols.contains_key("b"));
        assert!(!scope_l3.symbols.contains_key("c"));

        // Line 4: a, b, c defined
        let scope_l4 = scope_at_position(&artifacts, 4, 10);
        assert!(scope_l4.symbols.contains_key("a"));
        assert!(scope_l4.symbols.contains_key("b"));
        assert!(scope_l4.symbols.contains_key("c"));

        // Line 5: b, c removed, a remains
        let scope_l5 = scope_at_position(&artifacts, 5, 10);
        assert!(scope_l5.symbols.contains_key("a"));
        assert!(!scope_l5.symbols.contains_key("b"));
        assert!(!scope_l5.symbols.contains_key("c"));

        // Line 6: a re-defined again
        let scope_l6 = scope_at_position(&artifacts, 6, 10);
        assert!(scope_l6.symbols.contains_key("a"));
        assert!(!scope_l6.symbols.contains_key("b"));
        assert!(!scope_l6.symbols.contains_key("c"));
    }

    #[test]
    fn test_scope_removal_at_exact_position() {
        // Test scope at the exact position of the rm() call
        // Validates: Requirements 7.3, 7.4
        // Note: The scope resolution uses strict-before comparison, so at the exact position
        // of the rm() call, the removal is not yet processed.
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // At position (0, 10) - after x definition on line 0, x should be in scope
        let scope_before_rm_line = scope_at_position(&artifacts, 0, 10);
        assert!(scope_before_rm_line.symbols.contains_key("x"), 
            "x should be in scope on line 0 (before rm line)");

        // At position (1, 0) - at the start of rm(x) line, the removal is not processed
        // because scope resolution uses strict-before comparison
        let scope_at_rm_start = scope_at_position(&artifacts, 1, 0);
        assert!(scope_at_rm_start.symbols.contains_key("x"),
            "x should be in scope at rm() position (removal is processed strictly before)");

        // At position (1, 5) - after rm(x), x should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 1, 5);
        assert!(!scope_after_rm.symbols.contains_key("x"), 
            "x should NOT be in scope after rm(x) on the same line");
    }

    #[test]
    fn test_scope_with_deps_define_then_remove() {
        // Test scope_at_position_with_deps with define-then-remove
        // Validates: Requirements 7.3, 7.4
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let uri = test_uri();
        let artifacts = compute_artifacts(&uri, &tree, code);

        let get_artifacts = |u: &Url| -> Option<ScopeArtifacts> {
            if u == &uri { Some(artifacts.clone()) } else { None }
        };

        let resolve_path = |_path: &str, _from: &Url| -> Option<Url> { None };

        // After rm(), x should NOT be in scope
        let scope = scope_at_position_with_deps(&uri, 1, 10, &get_artifacts, &resolve_path, 10);
        assert!(!scope.symbols.contains_key("x"), 
            "x should NOT be in scope after rm() via scope_at_position_with_deps");
    }

    #[test]
    fn test_scope_with_deps_define_remove_define() {
        // Test scope_at_position_with_deps with define-remove-define sequence
        // Validates: Requirements 7.1
        let code = "x <- 1\nrm(x)\nx <- 2";
        let tree = parse_r(code);
        let uri = test_uri();
        let artifacts = compute_artifacts(&uri, &tree, code);

        let get_artifacts = |u: &Url| -> Option<ScopeArtifacts> {
            if u == &uri { Some(artifacts.clone()) } else { None }
        };

        let resolve_path = |_path: &str, _from: &Url| -> Option<Url> { None };

        // After second definition, x should be in scope
        let scope = scope_at_position_with_deps(&uri, 2, 10, &get_artifacts, &resolve_path, 10);
        assert!(scope.symbols.contains_key("x"), 
            "x should be in scope after re-definition via scope_at_position_with_deps");
    }

    // ============================================================================
    // Cross-file integration tests for removals (Task 7.2)
    // Validates: Requirements 6.1, 6.2
    // ============================================================================

    #[test]
    fn test_cross_file_source_then_remove_symbol() {
        // Test: Parent sources child that defines helper_func, then rm(helper_func)
        // helper_func should NOT be in scope after rm()
        // Validates: Requirements 6.1, 6.2
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: sources child.R, then removes helper_func
        let parent_code = "source(\"child.R\")\nrm(helper_func)";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines helper_func
        let child_code = "helper_func <- function() { 42 }";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Before rm() (line 0, after source), helper_func should be in scope
        let scope_before_rm = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(scope_before_rm.symbols.contains_key("helper_func"), 
            "helper_func should be in scope after source() but before rm()");

        // After rm() (line 1), helper_func should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_after_rm.symbols.contains_key("helper_func"), 
            "helper_func should NOT be in scope after rm()");

        // At end of file, helper_func should NOT be in scope
        let scope_eof = scope_at_position_with_graph(
            &parent_uri, 10, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_eof.symbols.contains_key("helper_func"), 
            "helper_func should NOT be in scope at end of file after rm()");
    }

    #[test]
    fn test_cross_file_source_then_remove_multiple_symbols() {
        // Test: Parent sources child that defines multiple symbols, then rm() some of them
        // Validates: Requirements 6.1, 6.2
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: sources child.R, then removes func_a and func_b
        let parent_code = "source(\"child.R\")\nrm(func_a, func_b)";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines func_a, func_b, func_c
        let child_code = "func_a <- function() { 1 }\nfunc_b <- function() { 2 }\nfunc_c <- function() { 3 }";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Before rm() (line 0, after source), all three functions should be in scope
        let scope_before_rm = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(scope_before_rm.symbols.contains_key("func_a"), "func_a should be in scope before rm()");
        assert!(scope_before_rm.symbols.contains_key("func_b"), "func_b should be in scope before rm()");
        assert!(scope_before_rm.symbols.contains_key("func_c"), "func_c should be in scope before rm()");

        // After rm() (line 1), func_a and func_b should NOT be in scope, but func_c should be
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_after_rm.symbols.contains_key("func_a"), "func_a should NOT be in scope after rm()");
        assert!(!scope_after_rm.symbols.contains_key("func_b"), "func_b should NOT be in scope after rm()");
        assert!(scope_after_rm.symbols.contains_key("func_c"), "func_c should still be in scope after rm()");
    }

    #[test]
    fn test_cross_file_backward_directive_with_removal_in_parent() {
        // Test: Child file with backward directive sees parent's scope with removals applied
        // Parent: defines x, sources child, then rm(x)
        // Child: should see x in scope (because it's sourced before rm)
        // Validates: Requirements 6.1, 6.2, 6.3
        use crate::cross_file::types::{BackwardDirective, CallSiteSpec, CrossFileMetadata};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();

        // Parent code: defines x, sources child at line 1, then rm(x) at line 2
        let parent_code = "x <- 1\nsource(\"child.R\")\nrm(x)";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: uses x (has backward directive pointing to parent)
        let child_code = "# @lsp-sourced-by parent.R line=2\ny <- x + 1";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Child metadata with backward directive
        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Line(1), // 0-based line 1 (source("child.R"))
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // In child file, x should be in scope (parent's scope at call site line 1)
        // At line 1 in parent, x is defined but rm(x) hasn't happened yet
        let scope_in_child = scope_at_position_with_backward(
            &child_uri, 1, 10, &get_artifacts, &get_metadata, &resolve_path, 10, None
        );

        assert!(scope_in_child.symbols.contains_key("x"), 
            "x should be in scope in child (parent's scope at call site before rm)");
        assert!(scope_in_child.symbols.contains_key("y"), 
            "y should be in scope in child (local definition)");
    }

    #[test]
    fn test_cross_file_backward_directive_removal_before_call_site() {
        // Test: Parent removes symbol BEFORE sourcing child
        // Child should NOT see the removed symbol
        // Validates: Requirements 6.1, 6.2, 6.3
        use crate::cross_file::types::{BackwardDirective, CallSiteSpec, CrossFileMetadata};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();

        // Parent code: defines x, rm(x), then sources child
        let parent_code = "x <- 1\nrm(x)\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: has backward directive pointing to parent
        let child_code = "# @lsp-sourced-by parent.R line=3\ny <- 1";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Child metadata with backward directive
        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "parent.R".to_string(),
                call_site: CallSiteSpec::Line(2), // 0-based line 2 (source("child.R"))
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // In child file, x should NOT be in scope (removed before call site)
        let scope_in_child = scope_at_position_with_backward(
            &child_uri, 1, 10, &get_artifacts, &get_metadata, &resolve_path, 10, None
        );

        assert!(!scope_in_child.symbols.contains_key("x"), 
            "x should NOT be in scope in child (removed before call site in parent)");
        assert!(scope_in_child.symbols.contains_key("y"), 
            "y should be in scope in child (local definition)");
    }

    #[test]
    fn test_cross_file_source_remove_redefine() {
        // Test: Parent sources child, removes symbol, then redefines it locally
        // The local redefinition should be in scope
        // Validates: Requirements 6.1, 6.2, 7.1
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: sources child.R, removes helper_func, then redefines it
        let parent_code = "source(\"child.R\")\nrm(helper_func)\nhelper_func <- function() { 99 }";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines helper_func
        let child_code = "helper_func <- function() { 42 }";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After source() but before rm() (line 0), helper_func from child should be in scope
        let scope_after_source = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(scope_after_source.symbols.contains_key("helper_func"), 
            "helper_func should be in scope after source()");

        // After rm() but before redefinition (line 1), helper_func should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_after_rm.symbols.contains_key("helper_func"), 
            "helper_func should NOT be in scope after rm()");

        // After redefinition (line 2), helper_func should be in scope again
        let scope_after_redef = scope_at_position_with_graph(
            &parent_uri, 2, 40, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(scope_after_redef.symbols.contains_key("helper_func"), 
            "helper_func should be in scope after local redefinition");

        // Verify the redefined symbol is from parent, not child
        let symbol = scope_after_redef.symbols.get("helper_func").unwrap();
        assert_eq!(symbol.source_uri, parent_uri, 
            "helper_func should be from parent after redefinition");
    }

    #[test]
    fn test_cross_file_removal_with_list_argument() {
        // Test: Parent sources child, then removes symbols using list= argument
        // Validates: Requirements 3.1, 3.2, 6.1, 6.2
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: sources child.R, then removes symbols using list=
        let parent_code = "source(\"child.R\")\nrm(list = c(\"func_a\", \"func_b\"))";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines func_a, func_b, func_c
        let child_code = "func_a <- function() { 1 }\nfunc_b <- function() { 2 }\nfunc_c <- function() { 3 }";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After rm(list=...) (line 1), func_a and func_b should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 40, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_after_rm.symbols.contains_key("func_a"), 
            "func_a should NOT be in scope after rm(list=...)");
        assert!(!scope_after_rm.symbols.contains_key("func_b"), 
            "func_b should NOT be in scope after rm(list=...)");
        assert!(scope_after_rm.symbols.contains_key("func_c"), 
            "func_c should still be in scope after rm(list=...)");
    }

    #[test]
    fn test_cross_file_removal_does_not_affect_child_scope() {
        // Test: Parent removes symbol, but child file's own scope is unaffected
        // Validates: Requirements 6.1, 6.2
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: sources child.R, then removes helper_func
        let parent_code = "source(\"child.R\")\nrm(helper_func)";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines helper_func
        let child_code = "helper_func <- function() { 42 }";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // In child file, helper_func should still be in scope (child's own definition)
        let scope_in_child = scope_at_position_with_graph(
            &child_uri, 0, 40, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(scope_in_child.symbols.contains_key("helper_func"), 
            "helper_func should be in scope in child file (its own definition)");

        // In parent file after rm(), helper_func should NOT be in scope
        let scope_in_parent = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_in_parent.symbols.contains_key("helper_func"), 
            "helper_func should NOT be in scope in parent after rm()");
    }

    #[test]
    fn test_cross_file_chained_sources_with_removal() {
        // Test: A sources B, B sources C, A removes symbol from C
        // Validates: Requirements 6.1, 6.2
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let uri_a = Url::parse("file:///project/a.R").unwrap();
        let uri_b = Url::parse("file:///project/b.R").unwrap();
        let uri_c = Url::parse("file:///project/c.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // A: sources B, then removes deep_func
        let code_a = "source(\"b.R\")\nrm(deep_func)";
        let tree_a = parse_r(code_a);
        let artifacts_a = compute_artifacts(&uri_a, &tree_a, code_a);

        // B: sources C
        let code_b = "source(\"c.R\")";
        let tree_b = parse_r(code_b);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, code_b);

        // C: defines deep_func
        let code_c = "deep_func <- function() { 42 }";
        let tree_c = parse_r(code_c);
        let artifacts_c = compute_artifacts(&uri_c, &tree_c, code_c);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let meta_a = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "b.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        let meta_b = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "c.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&uri_a, &meta_a, Some(&workspace_root), |_| None);
        graph.update_file(&uri_b, &meta_b, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else if uri == &uri_c { Some(artifacts_c.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &uri_a { Some(meta_a.clone()) }
            else if uri == &uri_b { Some(meta_b.clone()) }
            else { None }
        };

        // Before rm() in A (line 0), deep_func should be in scope (from C via B)
        let scope_before_rm = scope_at_position_with_graph(
            &uri_a, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(scope_before_rm.symbols.contains_key("deep_func"), 
            "deep_func should be in scope in A after source(B) which sources C");

        // After rm() in A (line 1), deep_func should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &uri_a, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        assert!(!scope_after_rm.symbols.contains_key("deep_func"), 
            "deep_func should NOT be in scope in A after rm()");
    }

    // ============================================================================
    // Unit tests for FunctionScopeTree (Interval Tree) - Task 1.6
    // Validates: Requirements 1.6, 2.2, 4.4
    // ============================================================================

    #[test]
    fn test_empty_tree_query() {
        // Test that query_point and query_innermost on empty tree return empty/None
        // Validates: Requirements 1.6 (empty tree handling)
        let tree = FunctionScopeTree::new();
        
        assert!(tree.is_empty(), "New tree should be empty");
        assert_eq!(tree.len(), 0, "New tree should have length 0");
        
        // query_point on empty tree should return empty Vec
        let results = tree.query_point(Position::new(5, 10));
        assert!(results.is_empty(), "query_point on empty tree should return empty Vec");
        
        // query_innermost on empty tree should return None
        let innermost = tree.query_innermost(Position::new(5, 10));
        assert!(innermost.is_none(), "query_innermost on empty tree should return None");
        
        // Also test with from_scopes with empty slice
        let tree_from_empty = FunctionScopeTree::from_scopes(&[]);
        assert!(tree_from_empty.is_empty(), "Tree from empty scopes should be empty");
        assert_eq!(tree_from_empty.len(), 0, "Tree from empty scopes should have length 0");
        
        let results_from_empty = tree_from_empty.query_point(Position::new(0, 0));
        assert!(results_from_empty.is_empty(), "query_point on tree from empty scopes should return empty Vec");
        
        let innermost_from_empty = tree_from_empty.query_innermost(Position::new(0, 0));
        assert!(innermost_from_empty.is_none(), "query_innermost on tree from empty scopes should return None");
    }

    /// Verifies that a FunctionScopeTree with a single interval reports containment correctly.
    ///
    /// This test constructs a tree containing one interval and asserts:
    /// - query_point returns the interval for positions inside or at the start boundary and returns no intervals for positions before or after the interval (including after the end column).
    /// - query_innermost returns the interval for an inside position and `None` for positions outside.
    ///
    /// # Examples
    ///
    /// ```
    /// let scopes = vec![(5, 0, 10, 20)];
    /// let tree = FunctionScopeTree::from_scopes(&scopes);
    /// assert_eq!(tree.len(), 1);
    /// let inside = tree.query_point(Position::new(7, 10));
    /// assert_eq!(inside.len(), 1);
    /// let none = tree.query_point(Position::new(3, 10));
    /// assert!(none.is_empty());
    /// let innermost = tree.query_innermost(Position::new(7, 10));
    /// assert!(innermost.is_some());
    /// ```
    #[test]
    fn test_single_interval_containment() {
        // Test basic containment check with one interval
        // Validates: Requirements 1.3 (point queries), 1.4 (return all containing intervals)
        
        // Create a tree with a single interval: lines 5-10, columns 0-20
        let scopes = vec![(5, 0, 10, 20)]; // (start_line, start_col, end_line, end_col)
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        assert!(!tree.is_empty(), "Tree should not be empty");
        assert_eq!(tree.len(), 1, "Tree should have 1 interval");
        
        // Position inside the interval
        let inside = tree.query_point(Position::new(7, 10));
        assert_eq!(inside.len(), 1, "Should find 1 interval for position inside");
        assert_eq!(inside[0].start, Position::new(5, 0));
        assert_eq!(inside[0].end, Position::new(10, 20));
        // Position before the interval (line 3)
        let before = tree.query_point(Position::new(3, 10));
        assert!(before.is_empty(), "Should find no intervals for position before");
        
        // Position after the interval (line 15)
        let after = tree.query_point(Position::new(15, 10));
        assert!(after.is_empty(), "Should find no intervals for position after");
        
        // Position on same line as start but before start column
        let same_line_before = tree.query_point(Position::new(5, 0)); // At start - should be included
        assert_eq!(same_line_before.len(), 1, "Position at start should be included (inclusive)");
        
        // Position on same line as end but after end column
        let same_line_after = tree.query_point(Position::new(10, 25));
        assert!(same_line_after.is_empty(), "Position after end column should not be included");
        
        // Test query_innermost with single interval
        let innermost_inside = tree.query_innermost(Position::new(7, 10));
        assert!(innermost_inside.is_some(), "query_innermost should return Some for position inside");
        assert_eq!(innermost_inside.unwrap().start, Position::new(5, 0));
        
        let innermost_outside = tree.query_innermost(Position::new(3, 10));
        assert!(innermost_outside.is_none(), "query_innermost should return None for position outside");
    }

    /// Verifies that function-scope intervals include their start and end positions.
    ///
    /// Constructs a tree with a single interval from (10,5) to (20,15) and asserts that
    /// positions exactly at the start and end (and positions inside the interval) are
    /// reported as contained, while positions just outside are not. Also checks that
    /// `query_innermost` returns `Some` at the boundary positions.
    ///
    /// # Examples
    ///
    /// ```
    /// let scopes = vec![(10, 5, 20, 15)];
    /// let tree = FunctionScopeTree::from_scopes(&scopes);
    /// assert!(!tree.query_point(Position::new(10, 5)).is_empty());
    /// assert!(!tree.query_point(Position::new(20, 15)).is_empty());
    /// assert!(tree.query_point(Position::new(10, 4)).is_empty());
    /// assert!(tree.query_point(Position::new(20, 16)).is_empty());
    /// ```
    #[test]
    fn test_boundary_positions_inclusive() {
        // Test that positions exactly at start/end are included (inclusive boundaries)
        // Validates: Requirements 4.2 (inclusive boundaries)
        
        // Create interval from (10, 5) to (20, 15)
        let scopes = vec![(10, 5, 20, 15)];
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        // Test exact start position - should be included
        let at_start = tree.query_point(Position::new(10, 5));
        assert_eq!(at_start.len(), 1, "Position at exact start should be included");
        
        // Test exact end position - should be included
        let at_end = tree.query_point(Position::new(20, 15));
        assert_eq!(at_end.len(), 1, "Position at exact end should be included");
        
        // Test one position before start (same line, column - 1)
        let before_start = tree.query_point(Position::new(10, 4));
        assert!(before_start.is_empty(), "Position just before start should not be included");
        
        // Test one position after end (same line, column + 1)
        let after_end = tree.query_point(Position::new(20, 16));
        assert!(after_end.is_empty(), "Position just after end should not be included");
        
        // Test start line but different column (inside)
        let start_line_inside = tree.query_point(Position::new(10, 10));
        assert_eq!(start_line_inside.len(), 1, "Position on start line with column inside should be included");
        
        // Test end line but different column (inside)
        let end_line_inside = tree.query_point(Position::new(20, 10));
        assert_eq!(end_line_inside.len(), 1, "Position on end line with column inside should be included");
        
        // Test middle of interval
        let middle = tree.query_point(Position::new(15, 10));
        assert_eq!(middle.len(), 1, "Position in middle should be included");
        
        // Test query_innermost at boundaries
        let innermost_at_start = tree.query_innermost(Position::new(10, 5));
        assert!(innermost_at_start.is_some(), "query_innermost at start should return Some");
        
        let innermost_at_end = tree.query_innermost(Position::new(20, 15));
        assert!(innermost_at_end.is_some(), "query_innermost at end should return Some");
    }

    #[test]
    fn test_nested_intervals_innermost() {
        // Test innermost selection with nested scopes
        // Validates: Requirements 2.1 (select interval with latest start), 2.2 (return None when empty)
        
        // Create nested intervals:
        // Outer: lines 0-100
        // Middle: lines 10-50
        // Inner: lines 20-30
        let scopes = vec![
            (0, 0, 100, 0),   // Outer function
            (10, 0, 50, 0),   // Middle function (nested in outer)
            (20, 0, 30, 0),   // Inner function (nested in middle)
        ];
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        assert_eq!(tree.len(), 3, "Tree should have 3 intervals");
        
        // Query at position inside all three (line 25)
        let all_containing = tree.query_point(Position::new(25, 0));
        assert_eq!(all_containing.len(), 3, "Should find all 3 nested intervals");
        
        // query_innermost should return the innermost (latest start = line 20)
        let innermost = tree.query_innermost(Position::new(25, 0));
        assert!(innermost.is_some(), "Should find innermost interval");
        let innermost_interval = innermost.unwrap();
        assert_eq!(innermost_interval.start, Position::new(20, 0), 
            "Innermost should have start at line 20 (latest start)");
        assert_eq!(innermost_interval.end, Position::new(30, 0));
        
        // Query at position inside outer and middle but not inner (line 15)
        let two_containing = tree.query_point(Position::new(15, 0));
        assert_eq!(two_containing.len(), 2, "Should find 2 intervals at line 15");
        
        let innermost_at_15 = tree.query_innermost(Position::new(15, 0));
        assert!(innermost_at_15.is_some());
        assert_eq!(innermost_at_15.unwrap().start, Position::new(10, 0),
            "Innermost at line 15 should be middle function (start line 10)");
        
        // Query at position inside only outer (line 5)
        let one_containing = tree.query_point(Position::new(5, 0));
        assert_eq!(one_containing.len(), 1, "Should find 1 interval at line 5");
        
        let innermost_at_5 = tree.query_innermost(Position::new(5, 0));
        assert!(innermost_at_5.is_some());
        assert_eq!(innermost_at_5.unwrap().start, Position::new(0, 0),
            "Innermost at line 5 should be outer function (start line 0)");
        
        // Query at position outside all (line 150)
        let none_containing = tree.query_point(Position::new(150, 0));
        assert!(none_containing.is_empty(), "Should find no intervals at line 150");
        
        let innermost_at_150 = tree.query_innermost(Position::new(150, 0));
        assert!(innermost_at_150.is_none(), "query_innermost should return None at line 150");
    }

    /// Verifies interval-tree behavior with EOF sentinel and extreme Position values.
    ///
    /// This test asserts that Position::eof() and positions containing u32::MAX are
    /// recognized as EOF sentinels, and that the interval tree performs pure
    /// lexicographic comparisons when determining containment. It checks:
    /// - EOF positions compare after all normal positions and do not match normal scopes.
    /// - Positions with MAX column on an interior line compare inside the interval if the
    ///   line is within the interval range.
    /// - Positions with MAX column on the end line compare after the interval end.
    /// - Position::is_eof() correctly identifies EOF sentinels.
    ///
    /// # Examples
    ///
    /// ```
    /// let scopes = vec![(0,0,10,0), (20,0,30,0), (50,0,100,0)];
    /// let tree = FunctionScopeTree::from_scopes(&scopes);
    /// assert!(Position::eof().is_eof());
    /// assert!(tree.query_point(Position::new(50, u32::MAX)).len() == 1);
    /// ```
    #[test]
    fn test_eof_sentinel_positions() {
        // Test EOF positions (u32::MAX) behavior with interval tree
        // Validates: Requirements 4.4 (EOF sentinel handling)
        // 
        // Note: The interval tree itself uses pure lexicographic comparison.
        // EOF sentinel handling (skipping function scope matching for EOF positions)
        // is done at the scope resolution level, not in the interval tree.
        // This test verifies the interval tree's behavior with extreme positions.
        
        // Create some normal function scopes
        let scopes = vec![
            (0, 0, 10, 0),
            (20, 0, 30, 0),
            (50, 0, 100, 0),
        ];
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        // Query at full EOF position (u32::MAX, u32::MAX)
        let eof_pos = Position::eof();
        assert!(eof_pos.is_eof(), "Position::eof() should be recognized as EOF");
        assert!(eof_pos.is_full_eof(), "Position::eof() should be recognized as full EOF");
        
        // EOF position is lexicographically after all normal scopes
        let results_at_eof = tree.query_point(eof_pos);
        assert!(results_at_eof.is_empty(), 
            "Full EOF position should not match any normal function scopes (lexicographically after all)");
        
        let innermost_at_eof = tree.query_innermost(eof_pos);
        assert!(innermost_at_eof.is_none(),
            "query_innermost at full EOF should return None");
        
        // Test with just MAX line
        let max_line_pos = Position::new(u32::MAX, 0);
        assert!(max_line_pos.is_eof(), "Position with MAX line should be recognized as EOF");
        assert!(!max_line_pos.is_full_eof(), "Position with MAX line only should not be full EOF");
        
        let results_max_line = tree.query_point(max_line_pos);
        assert!(results_max_line.is_empty(),
            "Position with MAX line should not match normal scopes (line is after all scope ends)");
        
        // Test with MAX column on a line that's inside a scope
        // Position (50, u32::MAX) is lexicographically between (50, 0) and (100, 0)
        // because line 50 < line 100, so it IS inside the interval (50, 0) to (100, 0)
        let max_col_pos = Position::new(50, u32::MAX);
        assert!(max_col_pos.is_eof(), "Position with MAX column should be recognized as EOF");
        assert!(!max_col_pos.is_full_eof(), "Position with MAX column only should not be full EOF");
        
        // The interval tree correctly includes this position because lexicographically:
        // (50, 0) <= (50, MAX) <= (100, 0) is true (50 < 100 for line comparison)
        let results_max_col = tree.query_point(max_col_pos);
        assert_eq!(results_max_col.len(), 1,
            "Position (50, MAX) is lexicographically inside interval (50,0)-(100,0)");
        assert_eq!(results_max_col[0].start.line, 50);
        
        // Test MAX column on a line that's at the end of a scope
        // Position (100, u32::MAX) is lexicographically AFTER (100, 0)
        let max_col_at_end = Position::new(100, u32::MAX);
        let results_max_col_at_end = tree.query_point(max_col_at_end);
        assert!(results_max_col_at_end.is_empty(),
            "Position (100, MAX) is after interval end (100, 0)");
        
        // Verify Position::is_eof() works correctly
        assert!(!Position::new(0, 0).is_eof(), "Normal position should not be EOF");
        assert!(!Position::new(100, 50).is_eof(), "Normal position should not be EOF");
        assert!(Position::new(u32::MAX, 0).is_eof(), "MAX line should be EOF");
        assert!(Position::new(0, u32::MAX).is_eof(), "MAX column should be EOF");
        assert!(Position::new(u32::MAX, u32::MAX).is_eof(), "Both MAX should be EOF");
        assert!(!Position::new(u32::MAX, 0).is_full_eof(), "MAX line only should not be full EOF");
        assert!(!Position::new(0, u32::MAX).is_full_eof(), "MAX column only should not be full EOF");
        assert!(Position::new(u32::MAX, u32::MAX).is_full_eof(), "Both MAX should be full EOF");
    }

    /// Verifies that a FunctionScopeTree correctly handles multiple disjoint (non-overlapping) intervals.
    ///
    /// Ensures that point queries return the single containing interval for positions inside each interval,
    /// return no intervals for positions in the gaps (or before/after all intervals), and that `query_innermost`
    /// returns the innermost interval when one exists.
    ///
    /// # Examples
    ///
    /// ```
    /// let scopes = vec![
    ///     (0, 0, 10, 0),
    ///     (20, 0, 30, 0),
    ///     (50, 0, 60, 0),
    ///     (100, 0, 110, 0),
    /// ];
    /// let tree = FunctionScopeTree::from_scopes(&scopes);
    ///
    /// // inside an interval
    /// let in_first = tree.query_point(Position::new(5, 0));
    /// assert_eq!(in_first.len(), 1);
    /// assert_eq!(in_first[0].start.line, 0);
    ///
    /// // in a gap
    /// let in_gap = tree.query_point(Position::new(15, 0));
    /// assert!(in_gap.is_empty());
    ///
    /// // innermost query
    /// let innermost = tree.query_innermost(Position::new(5, 0)).unwrap();
    /// assert_eq!(innermost.start.line, 0);
    /// ```
    #[test]
    fn test_non_overlapping_intervals() {
        // Test multiple disjoint intervals
        // Validates: Requirements 1.3 (point queries), 1.4 (return all containing intervals)
        
        // Create non-overlapping intervals
        let scopes = vec![
            (0, 0, 10, 0),    // First function: lines 0-10
            (20, 0, 30, 0),   // Second function: lines 20-30
            (50, 0, 60, 0),   // Third function: lines 50-60
            (100, 0, 110, 0), // Fourth function: lines 100-110
        ];
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        assert_eq!(tree.len(), 4, "Tree should have 4 intervals");
        
        // Query inside first interval
        let in_first = tree.query_point(Position::new(5, 0));
        assert_eq!(in_first.len(), 1, "Should find exactly 1 interval in first");
        assert_eq!(in_first[0].start.line, 0);
        
        // Query inside second interval
        let in_second = tree.query_point(Position::new(25, 0));
        assert_eq!(in_second.len(), 1, "Should find exactly 1 interval in second");
        assert_eq!(in_second[0].start.line, 20);
        
        // Query inside third interval
        let in_third = tree.query_point(Position::new(55, 0));
        assert_eq!(in_third.len(), 1, "Should find exactly 1 interval in third");
        assert_eq!(in_third[0].start.line, 50);
        
        // Query inside fourth interval
        let in_fourth = tree.query_point(Position::new(105, 0));
        assert_eq!(in_fourth.len(), 1, "Should find exactly 1 interval in fourth");
        assert_eq!(in_fourth[0].start.line, 100);
        
        // Query in gaps between intervals
        let in_gap_1 = tree.query_point(Position::new(15, 0)); // Between first and second
        assert!(in_gap_1.is_empty(), "Should find no intervals in gap between first and second");
        
        let in_gap_2 = tree.query_point(Position::new(40, 0)); // Between second and third
        assert!(in_gap_2.is_empty(), "Should find no intervals in gap between second and third");
        
        let in_gap_3 = tree.query_point(Position::new(80, 0)); // Between third and fourth
        assert!(in_gap_3.is_empty(), "Should find no intervals in gap between third and fourth");
        
        // Query before all intervals
        let before_all = tree.query_innermost(Position::new(15, 0));
        assert!(before_all.is_none(), "Should find no innermost in gap");
        
        // Query after all intervals
        let after_all = tree.query_point(Position::new(200, 0));
        assert!(after_all.is_empty(), "Should find no intervals after all");
        
        // Test query_innermost for each interval
        let innermost_first = tree.query_innermost(Position::new(5, 0));
        assert!(innermost_first.is_some());
        assert_eq!(innermost_first.unwrap().start.line, 0);
        
        let innermost_second = tree.query_innermost(Position::new(25, 0));
        assert!(innermost_second.is_some());
        assert_eq!(innermost_second.unwrap().start.line, 20);
    }

    #[test]
    fn test_interval_tree_with_same_start_positions() {
        // Test handling of intervals with identical start positions
        // Validates: Requirements 1.5 (handle identical start positions)
        
        // Create intervals with same start but different ends
        let scopes = vec![
            (10, 0, 20, 0),  // Same start, shorter
            (10, 0, 50, 0),  // Same start, longer
            (10, 0, 30, 0),  // Same start, medium
        ];
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        assert_eq!(tree.len(), 3, "Tree should have 3 intervals");
        
        // Query at position inside all three (line 15)
        let all_containing = tree.query_point(Position::new(15, 0));
        assert_eq!(all_containing.len(), 3, "Should find all 3 intervals at line 15");
        
        // Query at position inside only the longest (line 40)
        let only_longest = tree.query_point(Position::new(40, 0));
        assert_eq!(only_longest.len(), 1, "Should find only 1 interval at line 40");
        assert_eq!(only_longest[0].end.line, 50, "Should be the longest interval");
        
        // query_innermost should return one of them (all have same start)
        let innermost = tree.query_innermost(Position::new(15, 0));
        assert!(innermost.is_some());
        assert_eq!(innermost.unwrap().start, Position::new(10, 0),
            "Innermost should have start at line 10");
    }

    #[test]
    fn test_function_scope_interval_methods() {
        // Test FunctionScopeInterval helper methods
        // Validates: Requirements 4.1 (position comparison), 4.2 (inclusive boundaries)
        
        let interval = FunctionScopeInterval::new(
            Position::new(10, 5),
            Position::new(20, 15)
        );
        
        // Test contains() method
        assert!(interval.contains(Position::new(10, 5)), "Should contain start position");
        assert!(interval.contains(Position::new(20, 15)), "Should contain end position");
        assert!(interval.contains(Position::new(15, 10)), "Should contain middle position");
        assert!(!interval.contains(Position::new(10, 4)), "Should not contain position before start");
        assert!(!interval.contains(Position::new(20, 16)), "Should not contain position after end");
        assert!(!interval.contains(Position::new(5, 10)), "Should not contain position on earlier line");
        assert!(!interval.contains(Position::new(25, 10)), "Should not contain position on later line");
        
        // Test from_tuple() and to_tuple() round-trip
        let tuple = (10, 5, 20, 15);
        let from_tuple = FunctionScopeInterval::from_tuple(tuple);
        assert_eq!(from_tuple.start, Position::new(10, 5));
        assert_eq!(from_tuple.end, Position::new(20, 15));
        
        let back_to_tuple = from_tuple.as_tuple();
        assert_eq!(back_to_tuple, tuple, "Round-trip should preserve values");
    }

    #[test]
    fn test_position_ordering() {
        // Test Position lexicographic ordering
        // Validates: Requirements 4.1 (lexicographic ordering)
        
        // Same line, different columns
        assert!(Position::new(5, 0) < Position::new(5, 10));
        assert!(Position::new(5, 10) < Position::new(5, 20));
        
        // Different lines
        assert!(Position::new(5, 100) < Position::new(6, 0));
        assert!(Position::new(10, 0) < Position::new(20, 0));
        
        // Equal positions
        assert!(Position::new(5, 10) == Position::new(5, 10));
        assert!(!(Position::new(5, 10) < Position::new(5, 10)));
        assert!(!(Position::new(5, 10) > Position::new(5, 10)));
        
        // Test with large values
        assert!(Position::new(1000000, 50000) < Position::new(1000001, 0));
        
        // Test ordering is consistent with Ord trait
        let mut positions = vec![
            Position::new(10, 5),
            Position::new(5, 20),
            Position::new(5, 10),
            Position::new(10, 0),
            Position::new(5, 10), // duplicate
        ];
        positions.sort();
        
        assert_eq!(positions[0], Position::new(5, 10));
        assert_eq!(positions[1], Position::new(5, 10)); // duplicate
        assert_eq!(positions[2], Position::new(5, 20));
        assert_eq!(positions[3], Position::new(10, 0));
        assert_eq!(positions[4], Position::new(10, 5));
    }

    #[test]
    fn test_invalid_intervals_filtered() {
        // Test that invalid intervals (start > end) are filtered out
        // Validates: Error handling for invalid intervals
        
        let scopes = vec![
            (10, 0, 20, 0),  // Valid
            (30, 0, 25, 0),  // Invalid: end line < start line
            (40, 0, 50, 0),  // Valid
            (60, 10, 60, 5), // Invalid: same line but end column < start column
        ];
        let tree = FunctionScopeTree::from_scopes(&scopes);
        
        // Should only have 2 valid intervals
        assert_eq!(tree.len(), 2, "Tree should only have 2 valid intervals");
        
        // Query should only find valid intervals
        let in_first = tree.query_point(Position::new(15, 0));
        assert_eq!(in_first.len(), 1);
        assert_eq!(in_first[0].start.line, 10);
        
        let in_second = tree.query_point(Position::new(45, 0));
        assert_eq!(in_second.len(), 1);
        assert_eq!(in_second[0].start.line, 40);
        
        // Invalid interval ranges should not be found
        let in_invalid_1 = tree.query_point(Position::new(27, 0));
        assert!(in_invalid_1.is_empty(), "Should not find invalid interval");
        
        let in_invalid_2 = tree.query_point(Position::new(60, 7));
        assert!(in_invalid_2.is_empty(), "Should not find invalid interval");
    }

    // ============================================================================
    // Tests for PackageLoad events in compute_artifacts (Task 6.2)
    // Validates: Requirements 14.2, 14.4
    // ============================================================================

    #[test]
    fn test_package_load_event_global_scope() {
        // Test that library() calls at global scope create PackageLoad events with function_scope=None
        let code = "library(dplyr)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Find PackageLoad event
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, column, package, function_scope } = e {
                    Some((line, column, package, function_scope))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 1, "Should have exactly one PackageLoad event");
        let (line, _column, package, function_scope) = package_load_events[0];
        assert_eq!(*line, 0, "PackageLoad should be on line 0");
        assert_eq!(package, "dplyr", "Package name should be dplyr");
        assert!(function_scope.is_none(), "Global library() call should have function_scope=None");
    }

    #[test]
    fn test_package_load_event_inside_function() {
        // Test that library() calls inside a function create PackageLoad events with function_scope set
        let code = "my_func <- function() {\n  library(dplyr)\n  x <- 1\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Find PackageLoad event
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, column, package, function_scope } = e {
                    Some((*line, *column, package.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 1, "Should have exactly one PackageLoad event");
        let (line, _column, package, function_scope) = &package_load_events[0];
        assert_eq!(*line, 1, "PackageLoad should be on line 1");
        assert_eq!(package, "dplyr", "Package name should be dplyr");
        assert!(function_scope.is_some(), "library() inside function should have function_scope set");
    }

    #[test]
    fn test_package_load_multiple_calls() {
        // Test that multiple library() calls create multiple PackageLoad events in document order
        let code = "library(dplyr)\nlibrary(ggplot2)\nlibrary(tidyr)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Find PackageLoad events
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, package, .. } = e {
                    Some((*line, package.clone()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 3, "Should have three PackageLoad events");
        assert_eq!(package_load_events[0], (0, "dplyr".to_string()));
        assert_eq!(package_load_events[1], (1, "ggplot2".to_string()));
        assert_eq!(package_load_events[2], (2, "tidyr".to_string()));
    }

    #[test]
    fn test_package_load_require_call() {
        // Test that require() calls also create PackageLoad events
        let code = "require(dplyr)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { package, .. } = e {
                    Some(package.clone())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 1, "Should have one PackageLoad event");
        assert_eq!(package_load_events[0], "dplyr");
    }

    #[test]
    fn test_package_load_loadnamespace_call() {
        // Test that loadNamespace() calls also create PackageLoad events
        let code = r#"loadNamespace("dplyr")"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { package, .. } = e {
                    Some(package.clone())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 1, "Should have one PackageLoad event");
        assert_eq!(package_load_events[0], "dplyr");
    }

    #[test]
    fn test_package_load_timeline_sorted() {
        // Test that PackageLoad events are sorted correctly in the timeline with other events
        let code = "x <- 1\nlibrary(dplyr)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Extract event positions in order
        let event_positions: Vec<(u32, &str)> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Def { line, symbol, .. } => Some((*line, symbol.name.as_str())),
                ScopeEvent::PackageLoad { line, package, .. } => Some((*line, package.as_str())),
                _ => None,
            })
            .collect();

        // Should be in document order: x (line 0), dplyr (line 0), y (line 2)
        assert!(event_positions.len() >= 3, "Should have at least 3 events");
        
        // Verify ordering
        let mut prev_line = 0;
        for (line, _) in &event_positions {
            assert!(*line >= prev_line, "Events should be in document order");
            prev_line = *line;
        }
    }

    /// Verifies that a `library()` call inside a nested function is associated with the innermost function scope.
    ///
    /// This test parses a snippet with an outer and inner function, computes scope artifacts,
    /// locates the `PackageLoad` event for `library(dplyr)`, and asserts its `function_scope`
    /// is set and corresponds to the inner function.
    ///
    /// # Examples
    ///
    /// ```
    /// // Parses code with nested functions and ensures the package load is scoped to the inner function.
    /// let code = "outer <- function() {\n  inner <- function() {\n    library(dplyr)\n  }\n}";
    /// let tree = parse_r(code);
    /// let artifacts = compute_artifacts(&test_uri(), &tree, code);
    /// let package_load_events: Vec<_> = artifacts.timeline.iter()
    ///     .filter_map(|e| {
    ///         if let ScopeEvent::PackageLoad { function_scope, .. } = e {
    ///             Some(function_scope.clone())
    ///         } else {
    ///             None
    ///         }
    ///     })
    ///     .collect();
    /// assert_eq!(package_load_events.len(), 1);
    /// assert!(package_load_events[0].is_some());
    /// ```
    #[test]
    fn test_package_load_nested_function_scope() {
        // Test that library() in nested function gets the innermost function scope
        let code = "outer <- function() {\n  inner <- function() {\n    library(dplyr)\n  }\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Find PackageLoad event
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { function_scope, .. } = e {
                    Some(function_scope.clone())
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 1, "Should have one PackageLoad event");
        let function_scope = &package_load_events[0];
        assert!(function_scope.is_some(), "library() in nested function should have function_scope set");
        
        // The function_scope should be the inner function, not the outer one
        // We can verify this by checking that the scope interval is within the inner function
        if let Some(scope) = function_scope {
            // Inner function starts on line 1 and ends on line 3
            assert!(scope.start.line >= 1, "Function scope should start at or after inner function definition");
        }
    }

    #[test]
    fn test_package_load_mixed_global_and_function() {
        // Test that global and function-scoped library() calls are handled correctly
        let code = "library(dplyr)\nmy_func <- function() {\n  library(ggplot2)\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Find PackageLoad events
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { package, function_scope, .. } = e {
                    Some((package.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(package_load_events.len(), 2, "Should have two PackageLoad events");
        
        // First should be global (dplyr)
        let (pkg1, scope1) = &package_load_events[0];
        assert_eq!(pkg1, "dplyr");
        assert!(scope1.is_none(), "Global library(dplyr) should have function_scope=None");
        
        // Second should be function-scoped (ggplot2)
        let (pkg2, scope2) = &package_load_events[1];
        assert_eq!(pkg2, "ggplot2");
        assert!(scope2.is_some(), "library(ggplot2) inside function should have function_scope set");
    }

    // ============================================================================
    // Tests for scope_at_position_with_packages (Task 7.2)
    // Validates: Requirements 2.1, 2.2, 2.4, 2.5
    // ============================================================================

    /// Creates a lookup closure that returns a package's exported symbols.
    ///
    /// The returned closure maps a package name to a `HashSet<String>` containing
    /// the package's exports given by `packages`. If the package is not present,
    /// the closure returns an empty set.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashSet;
    ///
    /// let packages = [("pkgA", &["a", "b"][..]), ("pkgB", &["x"][..])];
    /// let get_exports = mock_package_exports(&packages);
    ///
    /// let a_exports: HashSet<String> = get_exports("pkgA");
    /// assert!(a_exports.contains("a"));
    /// assert!(a_exports.contains("b"));
    ///
    /// let missing: HashSet<String> = get_exports("unknown");
    /// assert!(missing.is_empty());
    /// ```
    fn mock_package_exports<'a>(packages: &'a [(&'a str, &'a [&'a str])]) -> impl Fn(&str) -> HashSet<String> + 'a {
        move |pkg: &str| {
            packages.iter()
                .find(|(name, _)| *name == pkg)
                .map(|(_, exports)| exports.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default()
        }
    }

    /// Helper function to create an empty base exports set for tests
    fn empty_base_exports() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn test_scope_with_packages_before_library_call() {
        // Requirement 2.1: Scope at position before library() call SHALL NOT include package exports
        let code = "x <- 1\nlibrary(dplyr)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter", "select"])]);
        let base_exports = empty_base_exports();

        // Query at line 0 (before library call on line 1)
        let scope = scope_at_position_with_packages(&artifacts, 0, 10, &get_exports, &base_exports);

        // Should have x but NOT package exports
        assert!(scope.symbols.contains_key("x"), "x should be in scope");
        assert!(!scope.symbols.contains_key("mutate"), "mutate should NOT be in scope before library()");
        assert!(!scope.symbols.contains_key("filter"), "filter should NOT be in scope before library()");
    }

    #[test]
    fn test_scope_with_packages_after_library_call() {
        // Requirement 2.2: Scope at position after library() call SHALL include package exports
        let code = "x <- 1\nlibrary(dplyr)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter", "select"])]);
        let base_exports = empty_base_exports();

        // Query at line 2 (after library call on line 1)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Should have x, y, and package exports
        assert!(scope.symbols.contains_key("x"), "x should be in scope");
        assert!(scope.symbols.contains_key("y"), "y should be in scope");
        assert!(scope.symbols.contains_key("mutate"), "mutate should be in scope after library()");
        assert!(scope.symbols.contains_key("filter"), "filter should be in scope after library()");
        assert!(scope.symbols.contains_key("select"), "select should be in scope after library()");
    }

    #[test]
    fn test_scope_with_packages_at_library_call_line() {
        // Requirement 2.2: Package exports available at or after the library() call position
        let code = "library(dplyr)\nx <- mutate";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        // Query at line 0, after the library call ends
        let scope = scope_at_position_with_packages(&artifacts, 0, 20, &get_exports, &base_exports);

        // Package exports should be available
        assert!(scope.symbols.contains_key("mutate"), "mutate should be in scope at library() line");
        assert!(scope.symbols.contains_key("filter"), "filter should be in scope at library() line");
    }

    #[test]
    fn test_scope_with_packages_function_scoped_inside_function() {
        // Requirement 2.4: Function-scoped library() calls only available within that function
        let code = "my_func <- function() {\n  library(dplyr)\n  x <- mutate\n}\ny <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        // Query inside the function (line 2, after library call)
        let scope_inside = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Package exports should be available inside the function
        assert!(scope_inside.symbols.contains_key("mutate"), 
            "mutate should be in scope inside function after library()");
        assert!(scope_inside.symbols.contains_key("filter"), 
            "filter should be in scope inside function after library()");
    }

    #[test]
    fn test_scope_with_packages_function_scoped_outside_function() {
        // Requirement 2.4: Function-scoped library() calls NOT available outside that function
        let code = "my_func <- function() {\n  library(dplyr)\n  x <- mutate\n}\ny <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        // Query outside the function (line 4)
        let scope_outside = scope_at_position_with_packages(&artifacts, 4, 10, &get_exports, &base_exports);

        // Package exports should NOT be available outside the function
        assert!(!scope_outside.symbols.contains_key("mutate"), 
            "mutate should NOT be in scope outside function");
        assert!(!scope_outside.symbols.contains_key("filter"), 
            "filter should NOT be in scope outside function");
        // But y should be available
        assert!(scope_outside.symbols.contains_key("y"), "y should be in scope");
    }

    #[test]
    fn test_scope_with_packages_global_library_call() {
        // Requirement 2.5: Top-level library() calls available globally from that point forward
        let code = "library(dplyr)\nmy_func <- function() {\n  x <- mutate\n}\ny <- filter";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        // Query inside the function
        let scope_inside = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);
        assert!(scope_inside.symbols.contains_key("mutate"), 
            "Global package exports should be available inside function");

        // Query outside the function
        let scope_outside = scope_at_position_with_packages(&artifacts, 3, 10, &get_exports, &base_exports);
        assert!(scope_outside.symbols.contains_key("filter"), 
            "Global package exports should be available outside function");
    }

    #[test]
    fn test_scope_with_packages_multiple_packages() {
        // Test that multiple library() calls accumulate exports
        let code = "library(dplyr)\nlibrary(ggplot2)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[
            ("dplyr", &["mutate", "filter"]),
            ("ggplot2", &["ggplot", "aes"]),
        ]);
        let base_exports = empty_base_exports();

        // Query after both library calls
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Should have exports from both packages
        assert!(scope.symbols.contains_key("mutate"), "dplyr exports should be in scope");
        assert!(scope.symbols.contains_key("filter"), "dplyr exports should be in scope");
        assert!(scope.symbols.contains_key("ggplot"), "ggplot2 exports should be in scope");
        assert!(scope.symbols.contains_key("aes"), "ggplot2 exports should be in scope");
    }

    #[test]
    fn test_scope_with_packages_local_definition_takes_precedence() {
        // Test that local definitions take precedence over package exports
        let code = "library(dplyr)\nmutate <- function(x) x + 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        // Query after local definition
        let scope = scope_at_position_with_packages(&artifacts, 1, 50, &get_exports, &base_exports);

        // mutate should be the local definition, not the package export
        assert!(scope.symbols.contains_key("mutate"), "mutate should be in scope");
        let mutate_symbol = scope.symbols.get("mutate").unwrap();
        assert_eq!(mutate_symbol.kind, SymbolKind::Function, 
            "mutate should be a function (local definition)");
        assert!(!mutate_symbol.source_uri.as_str().starts_with("package:"), 
            "mutate should be from local file, not package");

        // filter should still be from the package
        assert!(scope.symbols.contains_key("filter"), "filter should be in scope");
    }

    #[test]
    fn test_scope_with_packages_package_uri_format() {
        // Test that package symbols have the correct source URI format
        let code = "library(dplyr)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate"])]);
        let base_exports = empty_base_exports();

        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        let mutate_symbol = scope.symbols.get("mutate").unwrap();
        assert_eq!(mutate_symbol.source_uri.as_str(), "package:dplyr", 
            "Package symbol should have package:name URI");
    }

    #[test]
    fn test_scope_with_packages_empty_exports() {
        // Test that packages with no exports don't cause issues
        let code = "library(emptypackage)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("emptypackage", &[])]);
        let base_exports = empty_base_exports();

        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        // Should just have x, no package exports
        assert!(scope.symbols.contains_key("x"), "x should be in scope");
        assert_eq!(scope.symbols.len(), 1, "Should only have x in scope");
    }

    #[test]
    fn test_scope_with_packages_unknown_package() {
        // Test that unknown packages (not in callback) don't cause issues
        let code = "library(unknownpkg)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Callback returns empty for unknown packages
        let get_exports = mock_package_exports(&[("dplyr", &["mutate"])]);
        let base_exports = empty_base_exports();

        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        // Should just have x, no package exports
        assert!(scope.symbols.contains_key("x"), "x should be in scope");
        assert!(!scope.symbols.contains_key("mutate"), "mutate should NOT be in scope");
    }

    #[test]
    fn test_scope_with_packages_require_call() {
        // Test that require() calls also make package exports available
        let code = "require(dplyr)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        assert!(scope.symbols.contains_key("mutate"), "mutate should be in scope after require()");
        assert!(scope.symbols.contains_key("filter"), "filter should be in scope after require()");
    }

    #[test]
    fn test_scope_with_packages_loadnamespace_call() {
        // Test that loadNamespace() calls also make package exports available
        let code = r#"loadNamespace("dplyr")
x <- 1"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate", "filter"])]);
        let base_exports = empty_base_exports();

        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        assert!(scope.symbols.contains_key("mutate"), "mutate should be in scope after loadNamespace()");
        assert!(scope.symbols.contains_key("filter"), "filter should be in scope after loadNamespace()");
    }

    // ============================================================================
    // Tests for base package exports (Task 7.3)
    // Validates: Requirements 6.3, 6.4
    // ============================================================================

    #[test]
    fn test_base_exports_always_available() {
        // Requirement 6.3: Base packages SHALL be available at all positions
        // Requirement 6.4: Base packages SHALL NOT require position-aware loading
        let code = "x <- 1\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[]);
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());
        base_exports.insert("cat".to_string());
        base_exports.insert("sum".to_string());

        // Query at the very beginning of the file
        let scope_start = scope_at_position_with_packages(&artifacts, 0, 0, &get_exports, &base_exports);
        assert!(scope_start.symbols.contains_key("print"), "print should be in scope at start");
        assert!(scope_start.symbols.contains_key("cat"), "cat should be in scope at start");
        assert!(scope_start.symbols.contains_key("sum"), "sum should be in scope at start");

        // Query at the end of the file
        let scope_end = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);
        assert!(scope_end.symbols.contains_key("print"), "print should be in scope at end");
        assert!(scope_end.symbols.contains_key("cat"), "cat should be in scope at end");
        assert!(scope_end.symbols.contains_key("sum"), "sum should be in scope at end");
    }

    /// Ensures base package exports are available in scope before any explicit `library()` call.
    ///
    /// This test verifies that symbols from the base environment (e.g., `print`) are present
    /// at a position earlier in the file than a subsequent `library()` invocation, while
    /// symbols provided by the later-loaded package (e.g., `mutate` from `dplyr`) are not.
    ///
    /// # Examples
    ///
    /// ```
    /// // given code with a base call and a later library() call
    /// let code = "x <- print(1)\nlibrary(dplyr)\ny <- 2";
    /// let tree = parse_r(code);
    /// let artifacts = compute_artifacts(&test_uri(), &tree, code);
    ///
    /// let get_exports = mock_package_exports(&[("dplyr", &["mutate"])]);
    /// let mut base_exports = std::collections::HashSet::new();
    /// base_exports.insert("print".to_string());
    ///
    /// let scope = scope_at_position_with_packages(&artifacts, 0, 5, &get_exports, &base_exports);
    /// assert!(scope.symbols.contains_key("print"));
    /// assert!(!scope.symbols.contains_key("mutate"));
    /// ```
    #[test]
    fn test_base_exports_available_before_any_library_call() {
        // Base exports should be available even before any library() call
        let code = "x <- print(1)\nlibrary(dplyr)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("dplyr", &["mutate"])]);
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());

        // Query at line 0 (before library call)
        let scope = scope_at_position_with_packages(&artifacts, 0, 5, &get_exports, &base_exports);
        assert!(scope.symbols.contains_key("print"), "print should be in scope before library()");
        assert!(!scope.symbols.contains_key("mutate"), "mutate should NOT be in scope before library()");
    }

    #[test]
    fn test_base_exports_have_package_base_uri() {
        // Base exports should have package:base URI
        let code = "x <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[]);
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());

        let scope = scope_at_position_with_packages(&artifacts, 0, 10, &get_exports, &base_exports);
        let print_symbol = scope.symbols.get("print").unwrap();
        assert_eq!(print_symbol.source_uri.as_str(), "package:base", 
            "Base export should have package:base URI");
    }

    #[test]
    fn test_local_definition_overrides_base_export() {
        // Local definitions should take precedence over base exports
        let code = "print <- function(x) x + 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[]);
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());

        // Query after local definition
        let scope = scope_at_position_with_packages(&artifacts, 0, 50, &get_exports, &base_exports);
        
        let print_symbol = scope.symbols.get("print").unwrap();
        assert_eq!(print_symbol.kind, SymbolKind::Function, 
            "print should be a function (local definition)");
        assert!(!print_symbol.source_uri.as_str().starts_with("package:"), 
            "print should be from local file, not base package");
    }

    #[test]
    fn test_package_export_overrides_base_export() {
        // Explicit package exports should take precedence over base exports
        // (This tests the case where a package re-exports or shadows a base function)
        let code = "library(mypkg)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[("mypkg", &["print"])]);
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());

        // Query after library call
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);
        
        let print_symbol = scope.symbols.get("print").unwrap();
        // The package export should override the base export
        assert_eq!(print_symbol.source_uri.as_str(), "package:mypkg", 
            "print should be from mypkg, not base package");
    }

    #[test]
    fn test_base_exports_available_inside_function() {
        // Base exports should be available inside function bodies
        let code = "my_func <- function() {\n  x <- print(1)\n}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[]);
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());

        // Query inside the function
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);
        assert!(scope.symbols.contains_key("print"), "print should be in scope inside function");
    }

    #[test]
    fn test_empty_base_exports() {
        // Test that empty base exports don't cause issues
        let code = "x <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let get_exports = mock_package_exports(&[]);
        let base_exports = empty_base_exports();

        let scope = scope_at_position_with_packages(&artifacts, 0, 10, &get_exports, &base_exports);
        assert!(scope.symbols.contains_key("x"), "x should be in scope");
        assert_eq!(scope.symbols.len(), 1, "Should only have x in scope");
    }

    // ============================================================================
    // Tests for cross-file package propagation (Task 9.1)
    // Validates: Requirements 5.1, 5.2, 5.3
    // ============================================================================

    #[test]
    fn test_cross_file_package_propagation_via_source() {
        // Requirement 5.1: Parent file loads package before source() call,
        // package exports should be available in sourced file from the start
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: library(dplyr) at line 0, source("child.R") at line 1
        let parent_code = "library(dplyr)\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file: just uses mutate
        let child_code = "x <- mutate(df, y = 1)";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        // Create artifacts lookup
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query child's scope at position (0, 0) - should have inherited packages
        let scope = scope_at_position_with_graph(
            &child_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Child should have inherited dplyr from parent
        assert!(scope.inherited_packages.contains(&"dplyr".to_string()),
            "Child should inherit dplyr package from parent");
    }

    #[test]
    fn test_cross_file_package_propagation_respects_call_site() {
        // Requirement 5.1: Only packages loaded BEFORE source() call are propagated
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: source("child.R") at line 0, library(dplyr) at line 1
        let parent_code = "source(\"child.R\")\nlibrary(dplyr)";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file
        let child_code = "x <- 1";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query child's scope - should NOT have dplyr (loaded after source())
        let scope = scope_at_position_with_graph(
            &child_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Child should NOT have dplyr (it was loaded after source() call)
        assert!(!scope.inherited_packages.contains(&"dplyr".to_string()),
            "Child should NOT inherit dplyr (loaded after source() call)");
    }

    #[test]
    fn test_cross_file_package_propagation_multiple_packages() {
        // Requirement 5.3: Multiple packages loaded before source() are all propagated
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: library(dplyr) at line 0, library(ggplot2) at line 1, source() at line 2
        let parent_code = "library(dplyr)\nlibrary(ggplot2)\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file
        let child_code = "x <- 1";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 2,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query child's scope
        let scope = scope_at_position_with_graph(
            &child_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Child should have both packages
        assert!(scope.inherited_packages.contains(&"dplyr".to_string()),
            "Child should inherit dplyr from parent");
        assert!(scope.inherited_packages.contains(&"ggplot2".to_string()),
            "Child should inherit ggplot2 from parent");
    }

    #[test]
    fn test_cross_file_package_propagation_function_scoped_not_propagated() {
        // Function-scoped package loads should NOT be propagated to child files
        // unless the source() call is within the same function
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: function with library(dplyr) inside, source() outside
        let parent_code = "f <- function() {\n  library(dplyr)\n}\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file
        let child_code = "x <- 1";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 3,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query child's scope
        let scope = scope_at_position_with_graph(
            &child_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Child should NOT have dplyr (it's function-scoped in parent)
        assert!(!scope.inherited_packages.contains(&"dplyr".to_string()),
            "Child should NOT inherit function-scoped dplyr from parent");
    }

    // ============================================================================
    // Tests for forward-only package propagation (Task 9.2)
    // Validates: Requirement 5.4
    // ============================================================================

    #[test]
    fn test_forward_only_package_propagation_child_packages_not_in_parent() {
        // Requirement 5.4: Packages loaded in a sourced file should NOT propagate
        // back to the parent file.
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: source("child.R") at line 0, then uses mutate at line 1
        let parent_code = "source(\"child.R\")\nmutate(df, y = 1)";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file: loads dplyr
        let child_code = "library(dplyr)\nx <- 1";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query parent's scope AFTER the source() call
        // The parent should NOT have dplyr in inherited_packages because
        // packages don't propagate backward from child to parent
        let scope = scope_at_position_with_graph(
            &parent_uri,
            1,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Parent should NOT have dplyr (it was loaded in child, not parent)
        // Requirement 5.4: Forward-only propagation
        assert!(!scope.inherited_packages.contains(&"dplyr".to_string()),
            "Parent should NOT inherit dplyr from child (forward-only propagation)");
    }

    #[test]
    fn test_forward_only_package_propagation_child_symbols_available_but_not_packages() {
        // Requirement 5.4: While symbols from child files ARE merged into parent scope,
        // packages loaded in child files should NOT be propagated back.
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: source("child.R") at line 0
        let parent_code = "source(\"child.R\")\ny <- helper_func()";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file: loads ggplot2 and defines helper_func
        let child_code = "library(ggplot2)\nhelper_func <- function() { 1 }";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query parent's scope after source() call
        let scope = scope_at_position_with_graph(
            &parent_uri,
            1,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Symbols from child SHOULD be available in parent
        assert!(scope.symbols.contains_key("helper_func"),
            "Parent should have helper_func from child (symbols propagate)");
        
        // But packages from child should NOT be in parent's inherited_packages
        // Requirement 5.4: Forward-only propagation
        assert!(!scope.inherited_packages.contains(&"ggplot2".to_string()),
            "Parent should NOT inherit ggplot2 from child (forward-only propagation)");
    }

    #[test]
    fn test_forward_only_package_propagation_grandchild_packages_not_in_grandparent() {
        // Requirement 5.4: Packages loaded in deeply nested sourced files
        // should NOT propagate back through the chain.
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Grandparent file: source("parent.R")
        let grandparent_code = "source(\"parent.R\")\nz <- 1";
        let grandparent_tree = parse_r(grandparent_code);
        let grandparent_uri = Url::parse("file:///project/grandparent.R").unwrap();
        let grandparent_artifacts = compute_artifacts(&grandparent_uri, &grandparent_tree, grandparent_code);
        
        // Parent file: source("child.R")
        let parent_code = "source(\"child.R\")\ny <- 1";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file: loads stringr
        let child_code = "library(stringr)\nx <- 1";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        
        let grandparent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "parent.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&grandparent_uri, &grandparent_meta, Some(&workspace_root), |_| None);
        
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &grandparent_uri {
                Some(grandparent_artifacts.clone())
            } else if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &grandparent_uri {
                Some(grandparent_meta.clone())
            } else if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query grandparent's scope after source() call
        let grandparent_scope = scope_at_position_with_graph(
            &grandparent_uri,
            1,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Grandparent should NOT have stringr (loaded in grandchild)
        // Requirement 5.4: Forward-only propagation
        assert!(!grandparent_scope.inherited_packages.contains(&"stringr".to_string()),
            "Grandparent should NOT inherit stringr from grandchild (forward-only propagation)");
        
        // Query parent's scope after source() call
        let parent_scope = scope_at_position_with_graph(
            &parent_uri,
            1,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Parent should also NOT have stringr (loaded in child)
        assert!(!parent_scope.inherited_packages.contains(&"stringr".to_string()),
            "Parent should NOT inherit stringr from child (forward-only propagation)");
    }

    #[test]
    fn test_forward_only_package_propagation_parent_packages_propagate_child_packages_dont() {
        // Combined test: Parent's packages propagate to child, but child's packages
        // don't propagate back to parent.
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        
        // Parent file: library(dplyr), source("child.R")
        let parent_code = "library(dplyr)\nsource(\"child.R\")\nz <- 1";
        let parent_tree = parse_r(parent_code);
        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Child file: loads ggplot2
        let child_code = "library(ggplot2)\nx <- 1";
        let child_tree = parse_r(child_code);
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);
        
        let workspace_root = Url::parse("file:///project").unwrap();
        
        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);
        
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };
        
        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri {
                Some(parent_meta.clone())
            } else {
                None
            }
        };
        
        // Query child's scope - should have dplyr from parent
        let child_scope = scope_at_position_with_graph(
            &child_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Child SHOULD have dplyr (propagated from parent)
        assert!(child_scope.inherited_packages.contains(&"dplyr".to_string()),
            "Child should inherit dplyr from parent (forward propagation works)");
        
        // Query parent's scope after source() call
        let parent_scope = scope_at_position_with_graph(
            &parent_uri,
            2,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            10,
        );
        
        // Parent should NOT have ggplot2 (loaded in child)
        // Requirement 5.4: Forward-only propagation
        assert!(!parent_scope.inherited_packages.contains(&"ggplot2".to_string()),
            "Parent should NOT inherit ggplot2 from child (forward-only propagation)");
    }

    // ============================================================================
    // Tests for loaded_packages field in ScopeAtPosition (Task 10.2)
    // Validates: Requirements 8.1, 8.3, 8.4
    // ============================================================================

    #[test]
    fn test_loaded_packages_position_aware() {
        // Requirement 8.3: Symbol used BEFORE package is loaded should be flagged
        // This test verifies that loaded_packages is populated correctly based on position
        let code = "x <- mutate(df)\nlibrary(dplyr)\ny <- filter(df)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Query at line 0 (before library call) - should NOT have dplyr in loaded_packages
        let scope_before = scope_at_position(&artifacts, 0, 10);
        assert!(!scope_before.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should NOT be in loaded_packages before library() call");

        // Query at line 2 (after library call) - should have dplyr in loaded_packages
        let scope_after = scope_at_position(&artifacts, 2, 10);
        assert!(scope_after.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should be in loaded_packages after library() call");
    }

    #[test]
    fn test_loaded_packages_multiple_packages() {
        // Test that multiple packages are tracked correctly
        let code = "library(dplyr)\nlibrary(ggplot2)\nx <- 1";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Query at line 0 (after first library, before second)
        let scope_mid = scope_at_position(&artifacts, 0, 20);
        assert!(scope_mid.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should be in loaded_packages after first library() call");
        assert!(!scope_mid.loaded_packages.contains(&"ggplot2".to_string()),
            "ggplot2 should NOT be in loaded_packages before second library() call");

        // Query at line 2 (after both library calls)
        let scope_end = scope_at_position(&artifacts, 2, 10);
        assert!(scope_end.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should be in loaded_packages");
        assert!(scope_end.loaded_packages.contains(&"ggplot2".to_string()),
            "ggplot2 should be in loaded_packages");
    }

    #[test]
    fn test_loaded_packages_function_scoped() {
        // Requirement 2.4: library() inside function should only affect that function's scope
        let code = r#"my_func <- function() {
    library(dplyr)
    x <- mutate(df)
}
y <- filter(df)"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Query inside the function (line 2) - should have dplyr
        let scope_inside = scope_at_position(&artifacts, 2, 10);
        assert!(scope_inside.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should be in loaded_packages inside function");

        // Query outside the function (line 4) - should NOT have dplyr
        let scope_outside = scope_at_position(&artifacts, 4, 10);
        assert!(!scope_outside.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should NOT be in loaded_packages outside function (function-scoped)");
    }

    #[test]
    fn test_loaded_packages_with_require() {
        // Test that require() is also tracked
        let code = "require(dplyr)\nx <- mutate(df)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let scope = scope_at_position(&artifacts, 1, 10);
        assert!(scope.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should be in loaded_packages after require() call");
    }

    #[test]
    fn test_loaded_packages_with_loadnamespace() {
        // Test that loadNamespace() is also tracked
        let code = "loadNamespace(\"dplyr\")\nx <- mutate(df)";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        let scope = scope_at_position(&artifacts, 1, 10);
        assert!(scope.loaded_packages.contains(&"dplyr".to_string()),
            "dplyr should be in loaded_packages after loadNamespace() call");
    }
}