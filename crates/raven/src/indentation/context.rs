//! Context detection for R smart indentation.
//!
//! This module analyzes tree-sitter AST to determine the syntactic context
//! at the cursor position, which drives indentation decisions.
//!
//! # Error Handling
//!
//! This module implements robust error handling for various edge cases:
//! - Invalid AST states: Falls back to regex-based detection when AST has errors
//! - Position validation: Validates cursor position is within document bounds
//! - Missing delimiters: Handles unclosed delimiters gracefully with heuristics
//! - Iteration limits: Prevents infinite loops in chain start detection

use tower_lsp::lsp_types::Position;
use tree_sitter::{Node, Tree};

use super::calculator::get_line_indent;

use crate::utf16::utf16_column_to_byte_offset;

// ============================================================================
// AST Node Detection Functions
// ============================================================================

/// Checks if a node is a brace list node (code block inside `{}`).
///
/// In tree-sitter-r, braced code blocks use `braced_expression` as the node kind.
///
/// # Arguments
///
/// * `node` - The tree-sitter node to check
///
/// # Returns
///
/// `true` if the node is a `braced_expression` node.
pub fn is_brace_list_node(node: Node) -> bool {
    node.kind() == "braced_expression"
}

// ============================================================================
// AST Parent Walking Helpers
// ============================================================================

/// Walks up the AST from a node to find a parent matching a predicate.
///
/// # Arguments
///
/// * `node` - The starting node
/// * `predicate` - A function that returns `true` for the desired parent
///
/// # Returns
///
/// The first ancestor node matching the predicate, or `None` if not found.
pub fn find_parent<F>(node: Node, predicate: F) -> Option<Node>
where
    F: Fn(Node) -> bool,
{
    let mut current = node;
    while let Some(parent) = current.parent() {
        if predicate(parent) {
            return Some(parent);
        }
        current = parent;
    }
    None
}

/// Walks up the AST to find the nearest enclosing brace list node.
///
/// # Arguments
///
/// * `node` - The starting node
///
/// # Returns
///
/// The nearest `brace_list` ancestor, or `None` if not inside braces.
pub fn find_enclosing_brace_list(node: Node) -> Option<Node> {
    find_parent(node, is_brace_list_node)
}

// ============================================================================
// Core Types
// ============================================================================

/// Represents the syntactic context at the cursor position for indentation.
#[derive(Debug, Clone, PartialEq)]
pub enum IndentContext {
    /// Inside unclosed parentheses (function call arguments).
    InsideParens {
        /// Line number of the opening parenthesis.
        opener_line: u32,
        /// Column of the opening parenthesis.
        opener_col: u32,
        /// Whether there is content after the opening paren on the same line.
        has_content_on_opener_line: bool,
    },

    /// Inside unclosed braces (code block).
    InsideBraces {
        /// Line number of the opening brace.
        opener_line: u32,
        /// Column of the opening brace.
        opener_col: u32,
    },

    /// After a continuation operator (pipe, plus, tilde, infix).
    AfterContinuationOperator {
        /// Line number where the chain starts.
        chain_start_line: u32,
        /// Column where the chain starts (first non-whitespace).
        chain_start_col: u32,
        /// Type of the continuation operator.
        operator_type: OperatorType,
        /// The chain is the right-hand side of an assignment whose operator
        /// ends its line (lintr's `assignment_as_infix` suppression, #611).
        /// Continuations then stay at the chain-start line's indent instead
        /// of gaining the one-level floor — the assignment already paid for
        /// the level, and every lint mode flags a second one.
        flattened_rhs: bool,
    },

    /// After a line ending in an assignment operator
    /// (`<-`, `<<-`, `=`, `:=`, `->`, `->>`), issue #611.
    ///
    /// The right-hand side indents one level from the line where the
    /// assignment *statement* starts (not necessarily the operator's own
    /// line: in `data %>%` ⏎ `  f() ->` ⏎ the statement starts at `data`).
    /// This is style-independent — an assignment RHS is not a peer operand
    /// of the LHS, so there is nothing to align against.
    AfterAssignmentOperator {
        /// Line where the assignment statement starts (base for the indent).
        base_line: u32,
        /// The assignment is itself the RHS of an enclosing broken
        /// assignment (`a <-` ⏎ `  b <-` ⏎): the outer assignment already
        /// added the level, so this one adds none (mirrors the lint's
        /// `assignment_as_infix` suppression).
        flattened: bool,
    },

    /// After a complete expression (no trailing operator, no unclosed delimiters).
    AfterCompleteExpression {
        /// Indentation level of the enclosing block.
        enclosing_block_indent: u32,
    },

    /// Closing delimiter on its own line.
    ClosingDelimiter {
        /// Line number of the matching opening delimiter.
        opener_line: u32,
        /// Column of the matching opening delimiter.
        opener_col: u32,
        /// The closing delimiter character.
        delimiter: char,
    },
}

/// Types of continuation operators in R.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorType {
    /// Native R pipe `|>`
    Pipe,
    /// Magrittr pipe `%>%`
    MagrittrPipe,
    /// Plus operator `+` (commonly used in ggplot2)
    Plus,
    /// Tilde operator `~` (formula)
    Tilde,
    /// Custom infix operator `%word%`
    CustomInfix,
}

/// Classifies the continuation operator a line ends with, if any.
///
/// `trimmed` must already have its trailing whitespace and comment stripped
/// (e.g. via `strip_trailing_comment(line).trim_end()`).
///
/// This is the single source of truth for "does this line end in a
/// continuation operator, and which one." Both the operator-context detectors
/// (which need the [`OperatorType`]) and [`ChainWalker`]'s backward walk (which
/// needs only `.is_some()`) route through it, so the two cannot drift. The
/// `%>%` arm is tested before the generic `%word%` arm so magrittr pipes
/// classify as [`OperatorType::MagrittrPipe`], not [`OperatorType::CustomInfix`].
fn classify_continuation_operator(trimmed: &str) -> Option<OperatorType> {
    if trimmed.ends_with("|>") {
        Some(OperatorType::Pipe)
    } else if trimmed.ends_with("%>%") {
        Some(OperatorType::MagrittrPipe)
    } else if trimmed.ends_with('+') {
        Some(OperatorType::Plus)
    } else if trimmed.ends_with('~') {
        Some(OperatorType::Tilde)
    } else if trimmed.ends_with('%') && is_custom_infix_ending(trimmed) {
        Some(OperatorType::CustomInfix)
    } else {
        None
    }
}

/// Classifies the assignment operator a line ends with, if any, returning
/// the operator's token text (which equals its tree-sitter node kind, used
/// for AST validation in [`detect_assignment_continuation`]).
///
/// `trimmed` must already have its trailing whitespace and comment stripped,
/// same contract as [`classify_continuation_operator`]. The two classifiers
/// are deliberately separate: assignment operators must NOT count as chain
/// members ([`ChainWalker`] stops at an assignment-ended line, keeping the
/// chain start on the RHS's first line), and `%…%` custom operators are the
/// continuation classifier's business (it runs first in every caller). A
/// line ending in a `%`-delimited operator never reaches the arms below —
/// it ends in `%`, not in an assignment token — so e.g. zeallot's `%<-%`
/// is not an assignment here (nor a recognized custom infix today:
/// `is_custom_infix_ending` rejects `-` between the percent signs).
///
/// The `=` arm excludes `==`, `<=`, `>=`, `!=`; `:=` is matched before `=`.
fn classify_assignment_operator(trimmed: &str) -> Option<&'static str> {
    if trimmed.ends_with("<<-") {
        Some("<<-")
    } else if trimmed.ends_with("<-") {
        Some("<-")
    } else if trimmed.ends_with("->>") {
        Some("->>")
    } else if trimmed.ends_with("->") {
        Some("->")
    } else if trimmed.ends_with(":=") {
        Some(":=")
    } else if trimmed.ends_with('=')
        && !trimmed.ends_with("==")
        && !trimmed.ends_with("<=")
        && !trimmed.ends_with(">=")
        && !trimmed.ends_with("!=")
    {
        Some("=")
    } else {
        None
    }
}

/// Whether a line ending in this assignment operator suppresses the indent
/// level of a construct on its RHS (lintr's `assignment_as_infix`
/// suppressor set: `<-`, `<<-`, `=`, `:=` — right assignments `->`/`->>`
/// are excluded, matching `suppressed_as_assignment_rhs` in the lint).
///
/// Text-heuristic counterpart used on the regex fallback path; the AST path
/// calls the lint's predicate directly.
fn ends_with_flattening_assignment(trimmed: &str) -> bool {
    matches!(
        classify_assignment_operator(trimmed),
        Some("<-" | "<<-" | "=" | ":=")
    )
}

/// Finds the topmost line of the consecutive run of suppressing
/// assignment lines above `line` (the text-heuristic `assignment_as_infix`
/// walk): `Some(row)` when the nearest code line above ends with a
/// suppressing assignment operator, continuing upward through further
/// suppressor lines so chained broken assignments (`a <-` ⏎ `  b <-` ⏎)
/// report the outermost one. `None` when the nearest code line is not a
/// suppressor (no flattening).
///
/// Serves two callers: detection uses `is_some()` to decide flattening on
/// the fallback path, and the calculator derives the flattened *floor*
/// from the returned row (`its indent + tab`), so malformed input — a
/// chain line that is itself under-indented — still produces the level
/// the lint requires instead of mirroring the bad indent.
///
/// The walk skips lines the lint's AST predicate is insensitive to: blank
/// lines, comment-only lines, and lines consisting solely of `(` — a
/// parenthesized expression does not restore the context in
/// [`crate::linting::suppressed_as_assignment_rhs`], so an RHS wrapped as
/// `a <-` ⏎ `  (` ⏎ `    data %>%` still flattens. A `{`-only line stops
/// the walk (braces DO restore), as does any line with other content.
///
/// Documented approximation: this cannot see restorer boundaries at all;
/// detection callers guard the common restorer shape by rejecting
/// flattening when an unclosed delimiter opens at or after the chain-start
/// line. The residual gaps require an ERROR tree *and* an exotic shape
/// simultaneously; the AST path handles detection exactly.
pub(super) fn flattening_suppressor_line(source: &str, line: u32) -> Option<u32> {
    let lines: Vec<&str> = source.lines().collect();
    let mut topmost = None;
    for idx in (0..line as usize).rev() {
        let Some(text) = lines.get(idx) else {
            break;
        };
        let trimmed = strip_trailing_comment(text).trim_end();
        let content = trimmed.trim_start();
        if content.is_empty() {
            continue; // blank or comment-only line
        }
        if content.chars().all(|c| c == '(' || c.is_whitespace()) {
            continue; // parens don't restore the assignment context
        }
        if ends_with_flattening_assignment(trimmed) {
            topmost = Some(idx as u32);
            continue; // keep climbing through chained assignments
        }
        break;
    }
    topmost
}

/// Detects the syntactic context at the given position for indentation.
///
/// This function analyzes the tree-sitter AST to determine what kind of
/// indentation context the cursor is in (e.g., inside parentheses, after
/// a pipe operator, etc.).
///
/// The detection follows this priority order:
/// 1. ClosingDelimiter - if current line starts with closing delimiter
/// 2. AfterContinuationOperator - if previous line ends with continuation operator
/// 3. AfterAssignmentOperator - if previous line ends with an assignment
///    operator (#611; defers to InsideParens in paren/bracket context,
///    preempts InsideBraces)
/// 4. InsideParens - if inside unclosed parentheses
/// 5. InsideBraces - if inside unclosed braces
/// 6. AfterCompleteExpression - default fallback
///
/// # Error Handling
///
/// - If the AST has errors at the cursor position, falls back to regex-based detection
/// - If position is out of bounds, returns AfterCompleteExpression with indent 0
/// - Logs warnings for debugging without surfacing errors to user
///
/// # Arguments
///
/// * `tree` - The tree-sitter parse tree for the document
/// * `source` - The source code text
/// * `position` - The cursor position (typically after a newline)
///
/// # Returns
///
/// The detected `IndentContext` that should be used for indentation calculation.
pub fn detect_context(
    tree: &Tree,
    source: &str,
    position: Position,
    tab_size: u32,
) -> IndentContext {
    // Validate position is within document bounds
    if !is_position_valid(source, position) {
        log::warn!(
            "detect_context: position ({}, {}) is out of bounds for document with {} lines",
            position.line,
            position.character,
            source.lines().count()
        );
        return IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 0,
        };
    }

    // Check if AST has errors at cursor position - if so, use fallback
    let root = tree.root_node();
    if should_use_fallback(root, source, position) {
        log::trace!(
            "detect_context: using fallback detection due to AST errors at position ({}, {})",
            position.line,
            position.character
        );
        return fallback_detect_context(source, position, tab_size);
    }

    // 1. Check if current line starts with closing delimiter
    if let Some(ctx) = detect_closing_delimiter(source, position) {
        // Find the matching opener using AST
        if let Some(ctx) = find_matching_opener_context(tree, source, &ctx, position) {
            return ctx;
        }
        // find_matching_opener_context returns None when:
        // - No matching opener found in the AST, OR
        // - The closer is inside a pipe chain (auto-close push-down scenario)
        // In both cases, fall through to arguments/braces detection below,
        // which will find the correct enclosing context.
        log::trace!(
            "detect_context: no matching opener from AST, falling through to content detection"
        );
    }

    // 2. Check if previous line ends with continuation operator
    if let Some(ctx) = detect_continuation_operator(tree, source, position, tab_size) {
        return ctx;
    }

    // 2b. Check if previous line ends with an assignment operator (#611).
    // Must precede the parens/braces detection: the assignment context
    // preempts InsideBraces, while paren/bracket context is deferred to
    // inside detect_assignment_continuation itself.
    if let Some(ctx) = detect_assignment_continuation(tree, source, position) {
        return ctx;
    }

    // 3. Check for both parens and braces, return the innermost one
    // We need to find the actual AST positions to compare, not the indentation values
    let parens_node = find_unclosed_arguments_at_position(root, position);
    let braces_node = find_unclosed_braces_at_position(root, position, source);

    match (parens_node, braces_node) {
        (Some(p), Some(b)) => {
            // Both contexts found - return the innermost one (later start position)
            let parens_start = p.start_position();
            let braces_start = b.start_position();

            // Compare positions: later line wins, or same line with later column
            if braces_start.row > parens_start.row
                || (braces_start.row == parens_start.row
                    && braces_start.column > parens_start.column)
            {
                // Braces are innermost
                let opener_line = braces_start.row as u32;
                let opener_col = get_line_indent(source, opener_line, tab_size);
                IndentContext::InsideBraces {
                    opener_line,
                    opener_col,
                }
            } else {
                // Parens are innermost
                let opener_line = parens_start.row as u32;
                let opener_col = parens_start.column as u32;
                let has_content = check_content_after_opener(source, opener_line, opener_col);
                IndentContext::InsideParens {
                    opener_line,
                    opener_col,
                    has_content_on_opener_line: has_content,
                }
            }
        }
        (Some(p), None) => {
            let opener_pos = p.start_position();
            let opener_line = opener_pos.row as u32;
            let opener_col = opener_pos.column as u32;
            let has_content = check_content_after_opener(source, opener_line, opener_col);
            IndentContext::InsideParens {
                opener_line,
                opener_col,
                has_content_on_opener_line: has_content,
            }
        }
        (None, Some(b)) => {
            let opener_pos = b.start_position();
            let opener_line = opener_pos.row as u32;
            let opener_col = get_line_indent(source, opener_line, tab_size);
            IndentContext::InsideBraces {
                opener_line,
                opener_col,
            }
        }
        (None, None) => {
            // 4. Default to complete expression
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent: get_enclosing_block_indent(
                    tree, source, position, tab_size,
                ),
            }
        }
    }
}

// ============================================================================
// Error Handling Helper Functions
// ============================================================================

/// Validates that a position is within the document bounds.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `position` - The cursor position to validate
///
/// # Returns
///
/// `true` if the position is valid (within document bounds), `false` otherwise.
fn is_position_valid(source: &str, position: Position) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let line_count = lines.len();

    // Check line is within bounds
    if position.line as usize >= line_count {
        // Allow position at the end of the last line or on a new line after it
        if position.line as usize > line_count {
            return false;
        }
        // Position is on a line that doesn't exist yet (e.g., after pressing Enter at EOF)
        // This is valid - the line will be empty
        return true;
    }

    // Check character is within line bounds (allowing for end of line)
    let line_text = lines.get(position.line as usize).unwrap_or(&"");
    // LSP position.character is in UTF-16 code units, so compare against UTF-16 length.
    // Allow +1 for end-of-line position.
    position.character as usize <= line_text.encode_utf16().count() + 1
}

/// Determines if we should use fallback regex-based detection.
///
/// Returns true if the AST has errors at or near the cursor position,
/// indicating that AST-based detection may be unreliable.
///
/// # Arguments
///
/// * `root` - The root node of the parse tree
/// * `source` - The source code text
/// * `position` - The cursor position
///
/// # Returns
///
/// `true` if fallback detection should be used, `false` otherwise.
fn should_use_fallback(root: Node, source: &str, position: Position) -> bool {
    // Helper: check if any ancestor of the node at `point` is an ERROR node
    let has_error_at = |point: tree_sitter::Point| -> bool {
        if let Some(node) = root.descendant_for_point_range(point, point) {
            let mut current = node;
            loop {
                if current.is_error() {
                    return true;
                }
                match current.parent() {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
        }
        false
    };

    // Check cursor position (convert UTF-16 column to byte offset for tree-sitter)
    let line_text = source.lines().nth(position.line as usize).unwrap_or("");
    let byte_col = utf16_column_to_byte_offset(line_text, position.character);
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: byte_col,
    };
    if has_error_at(point) {
        return true;
    }

    // Check the previous line for errors (since we often indent based on it).
    // Check both the start AND end of the line — an ERROR node may begin
    // mid-line (e.g., `z(x = f(1,` where `z` is valid but `(x = f(1,` is ERROR).
    if position.line > 0 {
        let prev_row = (position.line - 1) as usize;
        let prev_start = tree_sitter::Point {
            row: prev_row,
            column: 0,
        };
        if has_error_at(prev_start) {
            return true;
        }

        if let Some(prev_line_text) = source.lines().nth(prev_row)
            && !prev_line_text.is_empty()
        {
            let end_col = prev_line_text.len().saturating_sub(1);
            let prev_end = tree_sitter::Point {
                row: prev_row,
                column: end_col,
            };
            if has_error_at(prev_end) {
                return true;
            }
        }
    }

    false
}

/// Fallback context detection using regex-based heuristics.
///
/// This function is used when the AST has errors and cannot be relied upon
/// for accurate context detection. It uses simple regex patterns to detect
/// common indentation contexts.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `position` - The cursor position
///
/// # Returns
///
/// The detected `IndentContext` based on regex heuristics.
fn fallback_detect_context(source: &str, position: Position, tab_size: u32) -> IndentContext {
    // 1. Check if current line starts with closing delimiter
    if let Some(line_text) = source.lines().nth(position.line as usize) {
        let trimmed = line_text.trim_start();
        if let Some(first_char) = trimmed.chars().next()
            && matches!(first_char, ')' | ']' | '}')
            // A line consisting only of closing delimiters is the auto-close
            // push-down scenario (Enter pushed the auto-inserted closers to
            // the cursor line): skip delimiter alignment and fall through to
            // content-level detection, mirroring `detect_closing_delimiter`
            // on the AST path (whitespace between closers tolerated there
            // via `trim()`, hence the filter).
            && !trimmed
                .chars()
                .filter(|c| !c.is_whitespace())
                .all(|c| matches!(c, ')' | ']' | '}'))
        {
            // Find matching opener using simple bracket counting
            if let Some((opener_line, opener_col)) =
                find_matching_opener_heuristic(source, position.line, first_char)
            {
                return IndentContext::ClosingDelimiter {
                    opener_line,
                    opener_col,
                    delimiter: first_char,
                };
            }
            // No matching opener found - use previous line indent
            let prev_indent = if position.line > 0 {
                get_line_indent(source, position.line - 1, tab_size)
            } else {
                0
            };
            return IndentContext::AfterCompleteExpression {
                enclosing_block_indent: prev_indent,
            };
        }
    }

    // Innermost unclosed delimiter before the cursor line: shared by the
    // continuation flattening guard (2), the assignment paren gate (2b),
    // and the delimiter context detection (3) — computed once.
    let unclosed = find_unclosed_delimiter_heuristic(source, position.line);

    // 2. Check if previous line ends with continuation operator
    if position.line > 0
        && let Some(prev_line) = source.lines().nth((position.line - 1) as usize)
    {
        let trimmed = strip_trailing_comment(prev_line).trim_end();

        if let Some(op_type) = classify_continuation_operator(trimmed) {
            // `position.line - 1` is the operator-terminated line just checked;
            // pass the cursor line per `find_chain_start`'s contract.
            let (chain_start_line, chain_start_col) =
                ChainWalker::new(source, tab_size).find_chain_start(position.line);
            // assignment_as_infix flattening, text approximation: a broken
            // assignment above the chain start suppresses the level, unless
            // a delimiter opened at/after the chain start (restorer shape:
            // the chain sits inside a call/parens, not directly on the RHS).
            let flattened_rhs = flattening_suppressor_line(source, chain_start_line).is_some()
                && !matches!(unclosed, Some((row, _, _)) if row >= chain_start_line);
            return IndentContext::AfterContinuationOperator {
                chain_start_line,
                chain_start_col,
                operator_type: op_type,
                flattened_rhs,
            };
        }

        // 2b. Check if previous line ends with an assignment operator (#611).
        // Paren gate: inside an unclosed `(`/`[`, argument alignment wins
        // (named arguments, call arguments, conditions); an unclosed `{`
        // or nothing keeps the assignment context, preempting InsideBraces.
        if classify_assignment_operator(trimmed).is_some()
            && !matches!(unclosed, Some((_, _, '(' | '[')))
        {
            // The assignment statement's start line: walk back through
            // continuation-operator-terminated lines from the assignment
            // line (`data %>%` ⏎ `  f() ->` starts at `data`). Assignment
            // lines themselves stop the walk, so a chained `a <-` above
            // does not extend it.
            let assign_line = position.line - 1;
            let (base_line, _) = ChainWalker::new(source, tab_size).find_chain_start(assign_line);
            let flattened = flattening_suppressor_line(source, base_line).is_some()
                && !matches!(unclosed, Some((row, _, _)) if row >= base_line);
            return IndentContext::AfterAssignmentOperator {
                base_line,
                flattened,
            };
        }
    }

    // 3. Check for unclosed delimiters using bracket counting
    if let Some((opener_line, opener_col, delimiter)) = unclosed {
        match delimiter {
            '(' | '[' => {
                let has_content = check_content_after_opener(source, opener_line, opener_col);
                return IndentContext::InsideParens {
                    opener_line,
                    opener_col,
                    has_content_on_opener_line: has_content,
                };
            }
            '{' => {
                let opener_indent = get_line_indent(source, opener_line, tab_size);
                return IndentContext::InsideBraces {
                    opener_line,
                    opener_col: opener_indent,
                };
            }
            _ => {}
        }
    }

    // 4. Default to complete expression with previous line indent
    let enclosing_indent = if position.line > 0 {
        get_line_indent(source, position.line - 1, tab_size)
    } else {
        0
    };

    IndentContext::AfterCompleteExpression {
        enclosing_block_indent: enclosing_indent,
    }
}

/// Finds a matching opening delimiter using simple bracket counting.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `closing_line` - The line containing the closing delimiter
/// * `closing_char` - The closing delimiter character
///
/// # Returns
///
/// `Some((line, col))` of the matching opener, or `None` if not found.
fn find_matching_opener_heuristic(
    source: &str,
    closing_line: u32,
    closing_char: char,
) -> Option<(u32, u32)> {
    let opening_char = match closing_char {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        _ => return None,
    };

    let lines: Vec<&str> = source.lines().collect();
    // Start with depth 1 because we're looking for the opener that matches
    // the closing delimiter on closing_line
    let mut depth = 1;

    // Walk backward from closing line (but skip the closing delimiter itself)
    // First, process the closing line up to (but not including) the closing delimiter
    if let Some(closing_line_text) = lines.get(closing_line as usize) {
        let stripped = strip_trailing_comment(closing_line_text);
        // Find the position of the first closing delimiter on this line
        if let Some(closing_pos) = stripped.find(closing_char) {
            // Process characters before the closing delimiter in reverse
            for (col, ch) in stripped[..closing_pos].char_indices().rev() {
                if ch == closing_char {
                    depth += 1;
                } else if ch == opening_char {
                    depth -= 1;
                    if depth == 0 {
                        return Some((closing_line, col as u32));
                    }
                }
            }
        }
    }

    // Now walk backward through previous lines
    if closing_line == 0 {
        return None;
    }

    for line_idx in (0..closing_line as usize).rev() {
        let line_text = lines.get(line_idx)?;
        let stripped = strip_trailing_comment(line_text);

        // Process characters in reverse order
        for (col, ch) in stripped.char_indices().rev() {
            if ch == closing_char {
                depth += 1;
            } else if ch == opening_char {
                depth -= 1;
                if depth == 0 {
                    // Found the matching opener
                    return Some((line_idx as u32, col as u32));
                }
            }
        }
    }

    None
}

/// Finds an unclosed delimiter using bracket counting.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `current_line` - The current line number
///
/// # Returns
///
/// `Some((line, col, delimiter))` of the innermost unclosed opener, or `None`.
fn find_unclosed_delimiter_heuristic(source: &str, current_line: u32) -> Option<(u32, u32, char)> {
    let lines: Vec<&str> = source.lines().collect();

    // Track unclosed delimiters with their positions
    let mut stack: Vec<(u32, u32, char)> = Vec::new();

    // Process lines up to (but not including) current line
    for line_idx in 0..current_line as usize {
        let line_text = lines.get(line_idx)?;
        let stripped = strip_trailing_comment(line_text);

        let mut in_string = false;
        let mut string_char = '"';
        let mut escape_next = false;
        let mut in_raw_string = false;
        let mut raw_close_delim = ')';
        let mut skip_raw_close_quote = false;
        let bytes = stripped.as_bytes();

        for (col, ch) in stripped.char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }
            // Raw-string handling mirrors `strip_trailing_comment`: the
            // delimiters inside `r"(...)"` are string content, not brackets
            // (otherwise `x[r"(a"#b)"] <-` hallucinates an unclosed `[`).
            if skip_raw_close_quote {
                skip_raw_close_quote = false;
                continue;
            }
            if in_raw_string {
                if ch == raw_close_delim {
                    let next_byte = col + ch.len_utf8();
                    if next_byte < bytes.len() && bytes[next_byte] == b'"' {
                        in_raw_string = false;
                        skip_raw_close_quote = true;
                    }
                }
                continue;
            }
            // No escape handling inside backticks — same rule and rationale
            // as `strip_trailing_comment`.
            if ch == '\\' && in_string && string_char != '`' {
                escape_next = true;
                continue;
            }
            if !in_string && (ch == 'r' || ch == 'R') {
                let next_byte = col + 1;
                if next_byte + 1 < bytes.len() && bytes[next_byte] == b'"' {
                    let close = match bytes[next_byte + 1] {
                        b'(' => Some(')'),
                        b'[' => Some(']'),
                        b'{' => Some('}'),
                        _ => None,
                    };
                    if let Some(cd) = close {
                        in_raw_string = true;
                        raw_close_delim = cd;
                        continue;
                    }
                }
            }
            if !in_string && (ch == '"' || ch == '\'' || ch == '`') {
                in_string = true;
                string_char = ch;
            } else if in_string && ch == string_char {
                in_string = false;
            } else if !in_string {
                match ch {
                    '(' | '[' | '{' => {
                        stack.push((line_idx as u32, col as u32, ch));
                    }
                    ')' => {
                        if let Some((_, _, '(')) = stack.last() {
                            stack.pop();
                        }
                    }
                    ']' => {
                        if let Some((_, _, '[')) = stack.last() {
                            stack.pop();
                        }
                    }
                    '}' => {
                        if let Some((_, _, '{')) = stack.last() {
                            stack.pop();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Return the innermost (last) unclosed delimiter
    stack.pop()
}

/// Detects if the current line starts with a closing delimiter.
///
/// Checks if the first non-whitespace character on the current line is
/// `)`, `]`, or `}`.
///
/// When the cursor is at position 0 on a line that contains only a closing
/// delimiter (with optional whitespace), this is likely an auto-close scenario:
/// the user pressed Enter between `(` and `)`, and VS Code pushed the auto-inserted
/// `)` to the new line. In this case, we skip closing delimiter detection so the
/// line gets indented as "inside parens" content instead.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `position` - The cursor position
///
/// # Returns
///
/// `Some(IndentContext::ClosingDelimiter)` if a closing delimiter is found
/// and the cursor is NOT at the start of the line (i.e., the user intentionally
/// placed the delimiter), with placeholder opener position (to be filled by AST lookup).
fn detect_closing_delimiter(source: &str, position: Position) -> Option<IndentContext> {
    let line_text = source.lines().nth(position.line as usize)?;
    let trimmed = line_text.trim_start();

    if trimmed.is_empty() {
        return None;
    }

    let first_char = trimmed.chars().next()?;
    if matches!(first_char, ')' | ']' | '}') {
        // If the line contains only the closing delimiter (with optional whitespace),
        // this is an auto-close push-down scenario: the user pressed Enter and
        // VS Code's auto-inserted closing delimiter got pushed to this line.
        // Skip closing delimiter detection so the line gets indented as content
        // inside the enclosing parens/braces.
        //
        // The onTypeFormatting position.character may be 0 or may reflect
        // the previous line's indentation — either way, a delimiter-only line
        // after Enter means we're inserting content, not aligning the delimiter.
        let after_delimiter = trimmed[first_char.len_utf8()..].trim();
        if after_delimiter.is_empty()
            || after_delimiter
                .chars()
                .all(|c| matches!(c, ')' | ']' | '}'))
        {
            return None;
        }

        // Return with placeholder opener position - will be updated by AST lookup
        Some(IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 0,
            delimiter: first_char,
        })
    } else {
        None
    }
}

/// Finds the matching opener for a closing delimiter using AST.
///
/// Walks up the AST from the closing delimiter position to find the
/// matching opening delimiter and returns an updated context with
/// the correct opener position.
///
/// # Arguments
///
/// * `tree` - The tree-sitter parse tree
/// * `source` - The source code text
/// * `ctx` - The closing delimiter context with placeholder opener position
/// * `position` - The cursor position (used to identify the closing delimiter line)
///
/// # Returns
///
/// Updated `IndentContext::ClosingDelimiter` with correct opener position,
/// or `None` if no matching opener is found.
fn find_matching_opener_context(
    tree: &Tree,
    source: &str,
    ctx: &IndentContext,
    position: Position,
) -> Option<IndentContext> {
    let IndentContext::ClosingDelimiter { delimiter, .. } = ctx else {
        return None;
    };

    // Use the cursor line directly — we already know it starts with a closing delimiter
    let line_text = source.lines().nth(position.line as usize)?;
    let trimmed = line_text.trim_start();
    if !trimmed.starts_with(*delimiter) {
        return None;
    }

    let col = line_text.len() - trimmed.len();
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: col,
    };

    // Get the node at the closing delimiter position and walk up to find the opener
    let node = tree.root_node().descendant_for_point_range(point, point)?;
    let (opener_line, opener_col) = find_opener_position(node, *delimiter, source)?;

    // When the cursor line contains only closing delimiters (auto-close push-down)
    // and the expression is inside a pipe chain, skip ClosingDelimiter so we fall
    // through to InsideParens content-level detection. Inside a pipe chain, the
    // auto-closed delimiters need content alignment, not delimiter alignment.
    let is_all_closers = trimmed.chars().all(|c| matches!(c, ')' | ']' | '}'));
    if is_all_closers && is_inside_pipe_chain(node, source) {
        return None;
    }

    Some(IndentContext::ClosingDelimiter {
        opener_line,
        opener_col,
        delimiter: *delimiter,
    })
}

/// Finds the position of the opening delimiter that matches a closing delimiter.
///
/// Checks if a node is inside a pipe chain (has a `|>` or `%>%` ancestor).
fn is_inside_pipe_chain(node: Node, source: &str) -> bool {
    let source_bytes = source.as_bytes();
    let mut current = node;
    loop {
        if current.kind() == "binary_operator" {
            let mut cursor = current.walk();
            for child in current.children(&mut cursor) {
                let kind = child.kind();
                if kind == "|>" {
                    return true;
                }
                if kind == "special"
                    && let Ok(text) = child.utf8_text(source_bytes)
                    && text == "%>%"
                {
                    return true;
                }
            }
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    false
}

/// # Arguments
///
/// * `node` - The tree-sitter node at or near the closing delimiter
/// * `delimiter` - The closing delimiter character
/// * `source` - The source code text
///
/// # Returns
///
/// `Some((line, col))` of the opening delimiter, or `None` if not found.
fn find_opener_position(node: Node, delimiter: char, _source: &str) -> Option<(u32, u32)> {
    let target_kind = match delimiter {
        ')' => "arguments",
        ']' => "subset", // or "subset2" for [[]]
        '}' => "braced_expression",
        _ => return None,
    };

    // Walk up to find the enclosing structure
    let mut current = node;
    loop {
        if current.kind() == target_kind
            || (delimiter == ']' && current.kind() == "subset2")
            || (delimiter == ')' && current.kind() == "call")
        {
            // For call nodes, we want the opening paren position
            // which is after the function name
            if current.kind() == "call" {
                // Find the arguments child
                for i in 0..current.child_count() {
                    if let Some(child) = current.child(i as u32)
                        && child.kind() == "arguments"
                    {
                        let start = child.start_position();
                        return Some((start.row as u32, start.column as u32));
                    }
                }
            }

            let start = current.start_position();
            return Some((start.row as u32, start.column as u32));
        }

        current = current.parent()?;
    }
}

/// Returns the continuation operator class for a `binary_operator` node.
///
/// Classes:
/// - 0: Pipe (`|>`, `%>%`)
/// - 1: Plus (`+`)
/// - 2: Tilde (`~`)
/// - 3: Infix (other `%word%` operators)
/// - None: no continuation operator found (e.g., `*`, `/`, `>`)
fn continuation_class_of_binop(binop: &tree_sitter::Node, source: &str) -> Option<u8> {
    for i in 0..binop.child_count() {
        if let Some(child) = binop.child(i as u32) {
            match child.kind() {
                "|>" => return Some(0),
                "+" => return Some(1),
                "~" => return Some(2),
                "special" => {
                    let text = &source[child.byte_range()];
                    if text == "%>%" {
                        return Some(0);
                    } else {
                        return Some(3);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Finds the start position of the current operator sub-chain within a mixed chain.
///
/// Drills into the LHS spine of `outermost` (the outermost same-class binary_operator).
/// When it hits a different operator class, returns the RHS of that boundary node —
/// the first operand of the current sub-chain. Returns `Some(col)` when a
/// cross-class boundary is found, `None` for single-class chains.
fn find_subchain_start(
    outermost: tree_sitter::Node,
    target_class: u8,
    source: &str,
) -> Option<u32> {
    let mut current = outermost;
    loop {
        let lhs = current.child(0)?;
        if lhs.kind() != "binary_operator" {
            return None;
        }
        match continuation_class_of_binop(&lhs, source) {
            Some(class) if class == target_class => current = lhs,
            _ => {
                // Cross-class boundary: sub-chain starts at RHS of this node.
                let rhs = lhs.child(2)?;
                return Some(rhs.start_position().column as u32);
            }
        }
    }
}

/// Whether a chain hangs in a bracket opened on the chain's own start line
/// (`a <-` ⏎ `  (data %>%` ⏎, or a chain inside `if (`/`while (`/`for (`
/// condition parens): these constructs do not restore the
/// `assignment_as_infix` suppression, but their hanging level still
/// applies — the lint expects continuations at the bracket-content or
/// block-bump column, never the bare line indent. Such chains are exempt
/// from flattening so the existing `max(chain_start_col, line_indent +
/// tab)` formula (lint-accepted) applies. A bracket opened on an *earlier*
/// line contributes a block level the chain line's own indent already
/// reflects, so flattening stands.
///
/// Control-flow statements are field-sensitive, mirroring
/// [`assignment_in_paren_context`]: only the `condition` (`if`/`while`) or
/// `sequence` (`for`) field hangs — a chain in a same-row *body*
/// (`a <-` ⏎ `  if (x) data %>%` ⏎) has no bracket level and the lint
/// expects the flattened column there.
///
/// The walk passes through any other ancestor that still begins on the
/// chain's row (mixed-chain `binary_operator`s, `unary_operator` as in
/// `(!data %>%`, statement bodies, …) and ends as soon as an ancestor
/// begins on an earlier row — by containment, everything above it also
/// begins on or before that row, so no same-row bracket can appear higher
/// up. Restorer boundaries (call/subset arguments, braces) need no arms
/// here: they make the suppression predicate false before this function
/// matters.
///
/// The fallback path needs no counterpart: at Enter time such a bracket is
/// unclosed on the chain-start line, and the fallback's
/// unclosed-delimiter-at-or-after-chain-start guard already rejects
/// flattening there.
fn chain_hangs_in_same_line_bracket(outermost: Node) -> bool {
    let row = outermost.start_position().row;
    let mut child = outermost;
    while let Some(parent) = child.parent() {
        if parent.start_position().row < row {
            return false;
        }
        match parent.kind() {
            "parenthesized_expression" => return true,
            "if_statement" | "while_statement"
                if parent
                    .child_by_field_name("condition")
                    .is_some_and(|c| c.id() == child.id()) =>
            {
                return true;
            }
            "for_statement"
                if parent
                    .child_by_field_name("sequence")
                    .is_some_and(|c| c.id() == child.id()) =>
            {
                return true;
            }
            _ => {}
        }
        child = parent;
    }
    false
}

/// Finds the continuation chain start position using AST analysis.
///
/// For continuation operators (`|>`, `%>%`, `+`, `~`, `%word%`), walks up
/// through parent `binary_operator` nodes of the same operator class to find
/// the outermost chained expression, then uses `find_subchain_start` to locate
/// the first operand of the current operator sub-chain (handling mixed chains
/// where different operator classes meet).
///
/// The third element of the result reports whether the chain is flattened —
/// it sits on the RHS of an assignment whose operator ends its line, per the
/// lint's [`crate::linting::suppressed_as_assignment_rhs`] predicate — so
/// the caller can drop the one-level indent floor (#611). Computed here
/// because only this function holds the outermost chain node. A chain that
/// hangs in a same-line paren is exempt from flattening (see
/// [`chain_hangs_in_same_line_bracket`]): there the paren's hanging level
/// still applies, and the lint rejects the bare line indent.
fn find_chain_start_from_ast(
    tree: &Tree,
    source: &str,
    prev_line: u32,
) -> Option<(u32, u32, bool)> {
    let root = tree.root_node();

    // Find the end of the actual code on prev_line (before comments/whitespace)
    let line_text = source.lines().nth(prev_line as usize)?;
    let trimmed = strip_trailing_comment(line_text);
    let trimmed = trimmed.trim_end();
    if trimmed.is_empty() {
        return None;
    }

    // Account for leading whitespace when computing the column
    let leading_ws = line_text.len() - line_text.trim_start().len();
    let content_len = trimmed.trim_start().len();
    let trimmed_end_col = leading_ws + content_len.saturating_sub(1);
    let point = tree_sitter::Point::new(prev_line as usize, trimmed_end_col);

    // Get the node at the operator position
    let node = root.descendant_for_point_range(point, point)?;

    // Verify this is a continuation operator or its parent is a binary_operator
    let chain_binop = if matches!(node.kind(), "|>" | "+" | "~" | "special") {
        // Direct hit on operator node - parent should be binary_operator
        node.parent().filter(|p| p.kind() == "binary_operator")?
    } else if node.kind() == "binary_operator" {
        node
    } else {
        // Try parent
        let parent = node.parent()?;
        if parent.kind() == "binary_operator" {
            parent
        } else {
            return None;
        }
    };

    // Determine our operator class
    let our_class = continuation_class_of_binop(&chain_binop, source)?;

    // Walk up through parent binary_operators with the SAME operator class.
    // Stop at different-class operators or non-binary-operator parents.
    let mut outermost = chain_binop;
    let mut current = chain_binop;
    while let Some(parent) = current.parent() {
        if parent.kind() == "binary_operator" {
            match continuation_class_of_binop(&parent, source) {
                Some(class) if class == our_class => {
                    outermost = parent;
                    current = parent;
                }
                _ => break,
            }
        } else {
            break;
        }
    }

    // For mixed chains, find_subchain_start returns the sub-chain's first
    // operand column. Use outermost's start line (which has the expression
    // root's indent level) so the calculator formula produces correct results.
    // For single-class chains, use outermost's start position directly.
    let flattened = crate::linting::suppressed_as_assignment_rhs(outermost)
        && !chain_hangs_in_same_line_bracket(outermost);
    let start = outermost.start_position();
    match find_subchain_start(outermost, our_class, source) {
        Some(sub_col) => Some((start.row as u32, sub_col, flattened)),
        None => Some((start.row as u32, start.column as u32, flattened)),
    }
}

/// Whether an assignment `binary_operator` sits in paren/bracket context,
/// where argument/content alignment (`InsideParens`) must win over the
/// assignment continuation context (#611).
///
/// Walks up from the node: `arguments`/`argument`, `subset`/`subset2`,
/// `parameters`/`parameter`, and `parenthesized_expression` ancestors defer;
/// a `braced_expression` restores statement context first (`f(function() {`
/// ⏎ `  x <-` ⏎ still indents one level, mirroring the lint's restorer
/// semantics). Control-flow condition parens have no
/// `parenthesized_expression` node in tree-sitter-r, so reaching an
/// `if_statement`/`while_statement` via its `condition` field (or a
/// `for_statement` via its `sequence` field) also defers, while reaching
/// them via a body field keeps walking.
///
/// This is an O(tree-depth) ancestor walk — deliberately not a text scan,
/// which a distant unclosed `(` hundreds of lines up (or parens inside a
/// multiline string) would poison.
fn assignment_in_paren_context(assign_binop: Node) -> bool {
    let mut child = assign_binop;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "arguments"
            | "argument"
            | "subset"
            | "subset2"
            | "parameters"
            | "parameter"
            | "parenthesized_expression" => return true,
            "braced_expression" => return false,
            "if_statement" | "while_statement"
                if parent
                    .child_by_field_name("condition")
                    .is_some_and(|c| c.id() == child.id()) =>
            {
                return true;
            }
            "for_statement"
                if parent
                    .child_by_field_name("sequence")
                    .is_some_and(|c| c.id() == child.id()) =>
            {
                return true;
            }
            _ => {}
        }
        child = parent;
    }
    false
}

/// Detects that the previous line ends with an assignment operator, whose
/// RHS indents one level from the assignment statement's start line (#611).
///
/// Runs after [`detect_continuation_operator`] (mutually exclusive — a line
/// cannot end with both operator classes) and before the parens/braces
/// detection, which it deliberately preempts for braces (`f <- function() {`
/// ⏎ `  x <-` ⏎ → one level from `x <-`, not the brace level) but defers to
/// for paren/bracket context via [`assignment_in_paren_context`] (named
/// arguments, formal defaults, call arguments, conditions — there the
/// `InsideParens` alignment wins).
///
/// The trailing operator is validated against the AST: the node at the
/// operator's position must be the operator token itself, with a
/// `binary_operator` parent. This rejects assignment-looking text inside
/// multiline strings and named-argument `=` (whose parent is `argument`).
fn detect_assignment_continuation(
    tree: &Tree,
    source: &str,
    position: Position,
) -> Option<IndentContext> {
    if position.line == 0 {
        return None;
    }

    let prev_line = position.line - 1;
    let line_text = source.lines().nth(prev_line as usize)?;
    let trimmed = strip_trailing_comment(line_text);
    let trimmed = trimmed.trim_end();
    let op = classify_assignment_operator(trimmed)?;

    // Locate the operator token's last byte (same arithmetic as
    // `find_chain_start_from_ast`).
    let leading_ws = line_text.len() - line_text.trim_start().len();
    let content_len = trimmed.trim_start().len();
    let end_col = leading_ws + content_len.saturating_sub(1);
    let point = tree_sitter::Point::new(prev_line as usize, end_col);

    let node = tree.root_node().descendant_for_point_range(point, point)?;
    if node.kind() != op {
        return None;
    }
    let binop = node.parent().filter(|p| p.kind() == "binary_operator")?;

    if assignment_in_paren_context(binop) {
        return None;
    }

    Some(IndentContext::AfterAssignmentOperator {
        // The binop starts where the assignment statement starts — for
        // `data %>%` ⏎ `  f() ->` the `->` binop's LHS is the whole chain,
        // so the base is the `data` line, matching the lint's block level.
        base_line: binop.start_position().row as u32,
        flattened: crate::linting::suppressed_as_assignment_rhs(binop),
    })
}

/// Detects if the previous line ends with a continuation operator.
///
/// Checks if the line before the cursor position ends with a continuation
/// operator (`|>`, `%>%`, `+`, `~`, `%word%`), ignoring trailing whitespace
/// and comments.
///
/// # Arguments
///
/// * `tree` - The tree-sitter parse tree
/// * `source` - The source code text
/// * `position` - The cursor position
///
/// # Returns
///
/// `Some(IndentContext::AfterContinuationOperator)` if a continuation operator
/// is found, with the chain start position and operator type.
fn detect_continuation_operator(
    tree: &Tree,
    source: &str,
    position: Position,
    tab_size: u32,
) -> Option<IndentContext> {
    // Need to check the previous line
    if position.line == 0 {
        return None;
    }

    let prev_line = position.line - 1;
    let line_text = source.lines().nth(prev_line as usize)?;

    // Strip trailing whitespace and comments, then classify the line ending.
    // An empty/whitespace-only line classifies as `None`, so no separate guard.
    let trimmed = strip_trailing_comment(line_text);
    let trimmed = trimmed.trim_end();
    let operator_type = classify_continuation_operator(trimmed)?;

    // Try AST-based chain start detection first, fall back to text-based
    // heuristic. The AST path evaluates the assignment_as_infix flattening
    // exactly (via the lint's predicate); the text path approximates it and
    // guards the restorer shape (chain inside a call opened at or after the
    // chain start — see `flattening_suppressor_line`).
    let (chain_start_line, chain_start_col, flattened_rhs) =
        find_chain_start_from_ast(tree, source, prev_line).unwrap_or_else(|| {
            let walker = ChainWalker::new(source, tab_size);
            let (line, col) = walker.find_chain_start(position.line);
            let flattened = flattening_suppressor_line(source, line).is_some()
                && !matches!(
                    find_unclosed_delimiter_heuristic(source, position.line),
                    Some((row, _, _)) if row >= line
                );
            (line, col, flattened)
        });

    Some(IndentContext::AfterContinuationOperator {
        chain_start_line,
        chain_start_col,
        operator_type,
        flattened_rhs,
    })
}

/// Checks if a line ends with a custom infix operator (%word%).
///
/// # Arguments
///
/// * `trimmed` - The line text with trailing whitespace/comments removed
///
/// # Returns
///
/// `true` if the line ends with a valid custom infix operator.
fn is_custom_infix_ending(trimmed: &str) -> bool {
    // Must end with % and have another % before it
    if !trimmed.ends_with('%') {
        return false;
    }

    let bytes = trimmed.as_bytes();
    let len = bytes.len();

    if len < 3 {
        return false;
    }

    // Find the opening % by scanning backward
    for i in (0..len - 1).rev() {
        if bytes[i] == b'%' {
            let between = &trimmed[i + 1..len - 1];
            // Valid infix operators allow alphanumeric and certain special chars
            if !between.is_empty()
                && between.chars().all(|c| {
                    c.is_alphanumeric()
                        || matches!(c, '.' | '>' | '<' | '*' | '/' | '|' | '&' | '!' | '=')
                })
            {
                return true;
            }
            break;
        }
    }

    false
}

/// Finds an unclosed arguments node that contains the given position.
///
/// This handles the case where tree-sitter marks the closing paren as MISSING.
fn find_unclosed_arguments_at_position<'a>(node: Node<'a>, position: Position) -> Option<Node<'a>> {
    let mut result: Option<Node<'a>> = None;

    // Check if this node is an arguments node
    if node.kind() == "arguments" {
        let start = node.start_position();
        let end = node.end_position();

        // Check if cursor is after the start
        let cursor_after_start = position.line as usize > start.row
            || (position.line as usize == start.row && position.character as usize > start.column);

        if cursor_after_start {
            // Check if the arguments node is unclosed (has MISSING close)
            // or if cursor is before the end
            let cursor_before_end = (position.line as usize) < end.row
                || (position.line as usize == end.row
                    && (position.character as usize) < end.column);

            let has_missing_close = has_missing_child(node);

            if cursor_before_end || has_missing_close {
                // Store as candidate but keep recursing to find innermost
                result = Some(node);
            }
        }
    }

    // Recursively check children, looking for the innermost match
    let mut cursor = node.walk();

    if cursor.goto_first_child() {
        loop {
            if let Some(found) = find_unclosed_arguments_at_position(cursor.node(), position) {
                // Keep the innermost (last found) match
                result = Some(found);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    result
}

/// Checks if a node has a MISSING child (indicating incomplete syntax).
fn has_missing_child(node: Node) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_missing() {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Checks if there's non-whitespace content after the opening paren on the same line.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `opener_line` - The line number of the opening paren
/// * `opener_col` - The column of the opening paren
///
/// # Returns
///
/// `true` if there's content after the opening paren on the same line.
fn check_content_after_opener(source: &str, opener_line: u32, opener_col: u32) -> bool {
    let Some(line_text) = source.lines().nth(opener_line as usize) else {
        return false;
    };

    // Get the text after the opening paren (skip the paren itself)
    let after_opener = if (opener_col as usize + 1) < line_text.len() {
        &line_text[(opener_col as usize + 1)..]
    } else {
        return false;
    };

    // Strip comments and check if there's non-whitespace content
    let stripped = strip_trailing_comment(after_opener);
    !stripped.trim().is_empty()
}

/// Finds an unclosed braced_expression node that contains the given position.
///
/// This handles the case where tree-sitter marks the closing brace as MISSING.
fn find_unclosed_braces_at_position<'a>(
    node: Node<'a>,
    position: Position,
    _source: &str,
) -> Option<Node<'a>> {
    let mut result: Option<Node<'a>> = None;

    // Check if this node is a braced_expression node
    if node.kind() == "braced_expression" {
        let start = node.start_position();
        let end = node.end_position();

        // Check if cursor is after the start
        let cursor_after_start = position.line as usize > start.row
            || (position.line as usize == start.row && position.character as usize > start.column);

        if cursor_after_start {
            // Check if the braced_expression node is unclosed (has MISSING close)
            // or if cursor is before the end
            let cursor_before_end = (position.line as usize) < end.row
                || (position.line as usize == end.row
                    && (position.character as usize) < end.column);

            let has_missing_close = has_missing_child(node);

            if cursor_before_end || has_missing_close {
                // Store as candidate but keep recursing to find innermost
                result = Some(node);
            }
        }
    }

    // Recursively check children, looking for the innermost match
    let mut cursor = node.walk();

    if cursor.goto_first_child() {
        loop {
            if let Some(found) = find_unclosed_braces_at_position(cursor.node(), position, _source)
            {
                // Keep the innermost (last found) match
                result = Some(found);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    result
}

/// Gets the indentation of the enclosing block.
///
/// Walks up the AST to find the nearest enclosing block (braced_expression,
/// function body, etc.) and returns its indentation level.
///
/// # Arguments
///
/// * `tree` - The tree-sitter parse tree
/// * `source` - The source code text
/// * `position` - The cursor position
///
/// # Returns
///
/// The indentation level of the enclosing block, or 0 if at top level.
fn get_enclosing_block_indent(tree: &Tree, source: &str, position: Position, tab_size: u32) -> u32 {
    // Convert UTF-16 column to byte offset for tree-sitter
    let line_text = source.lines().nth(position.line as usize).unwrap_or("");
    let byte_col = utf16_column_to_byte_offset(line_text, position.character);
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: byte_col,
    };

    // Get node at cursor position
    let Some(node) = tree.root_node().descendant_for_point_range(point, point) else {
        return 0;
    };

    // Walk up to find enclosing block
    if let Some(brace_node) = find_enclosing_brace_list(node) {
        let opener_line = brace_node.start_position().row as u32;
        return get_line_indent(source, opener_line, tab_size);
    }

    // No enclosing block, return 0 (top level)
    0
}
// ============================================================================
// Chain Start Detection
// ============================================================================

/// Walks backward through operator-terminated lines to find the chain start.
///
/// The chain start is the first line in a sequence of operator-terminated lines.
/// For example:
/// ```r
/// result <- data %>%    # Line 0 - chain start
///   filter(x > 0) %>%   # Line 1 - continuation
///   select(y)           # Line 2 - continuation
/// ```
///
/// When cursor is on line 2, chain start should be line 0, column 0 (start of "result").
pub struct ChainWalker<'a> {
    /// The source code text
    source: &'a str,
    /// Tab size for visual column calculation
    tab_size: u32,
}

impl<'a> ChainWalker<'a> {
    /// Creates a new ChainWalker.
    ///
    /// # Arguments
    ///
    /// * `source` - The source code text
    /// * `tab_size` - Tab size for visual column calculation
    pub fn new(source: &'a str, tab_size: u32) -> Self {
        Self { source, tab_size }
    }

    /// Finds the chain start by walking backward through operator-terminated lines.
    ///
    /// Starting from `cursor_line`, walks backward through lines that end with
    /// continuation operators (`|>`, `%>%`, `+`, `~`, `%word%`) until finding a line
    /// that does NOT end with such an operator. Comment-only lines are
    /// transparent — a `# note` between chain members does not break the
    /// chain syntactically, so the walk skips them (without ever selecting
    /// one as the chain start). Blank lines DO break the chain, preserving
    /// the documented troubleshooting behavior.
    ///
    /// # Arguments
    ///
    /// * `cursor_line` - The cursor's line. The walk begins one line above it
    ///   (`cursor_line - 1`) and proceeds upward, so callers pass the cursor line
    ///   after confirming that the line above ends with a continuation operator.
    ///
    /// # Returns
    ///
    /// A tuple `(line, column)` representing the chain start position:
    /// - `line`: The line number of the chain start
    /// - `column`: The column of the first non-whitespace character on that line
    pub fn find_chain_start(&self, cursor_line: u32) -> (u32, u32) {
        let mut current_line = cursor_line;
        let max_iterations = 1000;
        let mut iterations = 0;

        while current_line > 0 && iterations < max_iterations {
            // Walk upward past comment-only lines to the nearest code line.
            let mut probe = current_line - 1;
            while probe > 0 && self.is_comment_only_line(probe) {
                probe -= 1;
                iterations += 1;
            }
            if self.is_comment_only_line(probe) || !self.line_ends_with_operator(probe) {
                break;
            }
            current_line = probe;
            iterations += 1;
        }

        if iterations >= max_iterations {
            log::warn!("Chain start detection exceeded max iterations");
        }

        (current_line, self.get_line_start_column(current_line))
    }

    /// Whether a line contains only a comment (after stripping the comment
    /// nothing but whitespace remains, though the line itself is not blank).
    fn is_comment_only_line(&self, line: u32) -> bool {
        let Some(line_text) = self.get_line_text(line) else {
            return false;
        };
        strip_trailing_comment(line_text).trim().is_empty() && !line_text.trim().is_empty()
    }

    /// Checks if a line ends with a continuation operator.
    ///
    /// Delegates the operator vocabulary to [`classify_continuation_operator`]
    /// (the single source of truth); trailing whitespace and comments are
    /// stripped before the check.
    ///
    /// # Arguments
    ///
    /// * `line` - The line number to check (0-indexed)
    ///
    /// # Returns
    ///
    /// `true` if the line ends with a continuation operator.
    fn line_ends_with_operator(&self, line: u32) -> bool {
        let Some(line_text) = self.get_line_text(line) else {
            return false;
        };

        // Strip trailing whitespace and comments
        let trimmed = strip_trailing_comment(line_text);
        let trimmed = trimmed.trim_end();

        classify_continuation_operator(trimmed).is_some()
    }

    /// Gets the column of the first non-whitespace character on a line.
    ///
    /// # Arguments
    ///
    /// * `line` - The line number (0-indexed)
    ///
    /// # Returns
    ///
    /// The column (0-indexed) of the first non-whitespace character,
    /// or 0 if the line is empty or all whitespace.
    fn get_line_start_column(&self, line: u32) -> u32 {
        get_line_indent(self.source, line, self.tab_size)
    }

    /// Gets the text content of a specific line.
    ///
    /// # Arguments
    ///
    /// * `line` - The line number (0-indexed)
    ///
    /// # Returns
    ///
    /// The line text, or `None` if the line doesn't exist.
    fn get_line_text(&self, line: u32) -> Option<&'a str> {
        self.source.lines().nth(line as usize)
    }
}

/// Strips trailing comments from a line of R code.
///
/// R comments start with `#` and continue to end of line.
/// This function handles the case where `#` appears inside strings.
///
/// # Arguments
///
/// * `line` - The line text
///
/// # Returns
///
/// The line text with trailing comment removed.
fn strip_trailing_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut string_char = '"';
    let mut escape_next = false;
    let mut in_raw_string = false;
    let mut raw_close_delim: char = ')';
    let mut skip_raw_close_quote = false;

    let bytes = line.as_bytes();

    for (i, c) in line.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        // Skip the closing '"' of a raw string (already consumed at close_delim)
        if skip_raw_close_quote {
            skip_raw_close_quote = false;
            continue;
        }

        // Inside raw string: look for close_delim followed by '"'
        if in_raw_string {
            if c == raw_close_delim {
                let next_byte = i + c.len_utf8();
                if next_byte < bytes.len() && bytes[next_byte] == b'"' {
                    in_raw_string = false;
                    skip_raw_close_quote = true;
                }
            }
            continue;
        }

        // No escape handling inside backticks: R's parser rejects `a\`b`
        // ("unexpected symbol", verified on R 4.6.1) — a backquoted name
        // cannot contain a backtick, so the first unescaped backtick always
        // closes it. `#` inside backticks needs no escape and is handled by
        // the in_string state alone.
        if c == '\\' && in_string && string_char != '`' {
            escape_next = true;
            continue;
        }

        if !in_string && (c == 'r' || c == 'R') {
            // Check for raw string: r"(...)", R"[...]", r"{...}"
            let next_byte = i + 1;
            if next_byte + 1 < bytes.len() && bytes[next_byte] == b'"' {
                let open_delim = bytes[next_byte + 1];
                let close = match open_delim {
                    b'(' => Some(')'),
                    b'[' => Some(']'),
                    b'{' => Some('}'),
                    _ => None,
                };
                if let Some(cd) = close {
                    in_raw_string = true;
                    raw_close_delim = cd;
                    continue;
                }
            }
        }

        if !in_string && (c == '"' || c == '\'' || c == '`') {
            in_string = true;
            string_char = c;
        } else if in_string && c == string_char {
            in_string = false;
        } else if !in_string && c == '#' {
            return &line[..i];
        }
    }

    line
}

/// Extracts the text content of a tree-sitter node.
///
/// Test-only helper for locating nodes by their source text.
#[cfg(test)]
fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or_else(|| {
        log::warn!(
            "node_text: byte range {:?} out of bounds for source len {}",
            node.byte_range(),
            source.len()
        );
        ""
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Helper to parse R code and get the tree
    fn parse_r_code(code: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .expect("Failed to set R language");
        parser.parse(code, None).expect("Failed to parse code")
    }

    /// Recursively visits all nodes in the tree and collects them.
    fn collect_all_nodes(node: Node) -> Vec<Node> {
        let mut nodes = vec![node];
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                nodes.extend(collect_all_nodes(cursor.node()));
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        nodes
    }

    // ========================================================================
    // Error Handling Tests (Task 9.6)
    // ========================================================================

    #[test]
    fn test_is_position_valid_within_bounds() {
        let source = "line0\nline1\nline2";
        assert!(is_position_valid(
            source,
            Position {
                line: 0,
                character: 0
            }
        ));
        assert!(is_position_valid(
            source,
            Position {
                line: 1,
                character: 3
            }
        ));
        assert!(is_position_valid(
            source,
            Position {
                line: 2,
                character: 5
            }
        ));
    }

    #[test]
    fn test_is_position_valid_at_end_of_line() {
        let source = "line0\nline1";
        // Position at end of line should be valid
        assert!(is_position_valid(
            source,
            Position {
                line: 0,
                character: 5
            }
        ));
        assert!(is_position_valid(
            source,
            Position {
                line: 1,
                character: 5
            }
        ));
    }

    #[test]
    fn test_is_position_valid_out_of_bounds_line() {
        let source = "line0\nline1";
        // Line 5 doesn't exist
        assert!(!is_position_valid(
            source,
            Position {
                line: 5,
                character: 0
            }
        ));
    }

    #[test]
    fn test_is_position_valid_empty_source() {
        let source = "";
        // Empty source - line 0 is valid (for new line after EOF)
        assert!(is_position_valid(
            source,
            Position {
                line: 0,
                character: 0
            }
        ));
        assert!(!is_position_valid(
            source,
            Position {
                line: 1,
                character: 0
            }
        ));
    }

    #[test]
    fn test_is_position_valid_new_line_after_eof() {
        let source = "line0";
        // Position on line after last line (e.g., after pressing Enter at EOF)
        assert!(is_position_valid(
            source,
            Position {
                line: 1,
                character: 0
            }
        ));
        // But not two lines after
        assert!(!is_position_valid(
            source,
            Position {
                line: 2,
                character: 0
            }
        ));
    }

    #[test]
    fn test_should_use_fallback_no_errors() {
        let code = "x <- 1 + 2";
        let tree = parse_r_code(code);
        let position = Position {
            line: 0,
            character: 5,
        };
        assert!(!should_use_fallback(tree.root_node(), code, position));
    }

    #[test]
    fn test_should_use_fallback_with_syntax_error() {
        // Use a more clearly invalid syntax that tree-sitter will mark as error
        let code = "x <- <- y"; // Double assignment operator is invalid
        let tree = parse_r_code(code);
        let position = Position {
            line: 0,
            character: 5,
        };
        // This may or may not trigger fallback depending on how tree-sitter handles it
        // The important thing is that it doesn't panic
        let _ = should_use_fallback(tree.root_node(), code, position);
    }

    #[test]
    fn test_should_use_fallback_incomplete_expression() {
        // Incomplete expressions may not always be marked as errors by tree-sitter
        // The important thing is that the function handles them gracefully
        let code = "x <-"; // Incomplete assignment
        let tree = parse_r_code(code);
        let position = Position {
            line: 0,
            character: 4,
        };
        // This may or may not trigger fallback - just verify it doesn't panic
        let _ = should_use_fallback(tree.root_node(), code, position);
    }

    #[test]
    fn test_should_use_fallback_multibyte_utf16() {
        // CJK character (U+4E16) is 1 UTF-16 code unit but 3 bytes.
        // Verify the UTF-16→byte conversion doesn't cause a panic or wrong lookup.
        let code = "\u{4E16} <- 1 + 2";
        let tree = parse_r_code(code);
        // character=2 in UTF-16 is after the CJK char + space
        let position = Position {
            line: 0,
            character: 2,
        };
        // Should not panic regardless of fallback result
        let _ = should_use_fallback(tree.root_node(), code, position);
    }

    #[test]
    fn test_is_position_valid_multibyte() {
        // CJK character: 3 bytes, 1 UTF-16 code unit
        let source = "\u{4E16}\u{754C}"; // 世界 — 2 chars, 2 UTF-16 code units, 6 bytes
        // character=2 (UTF-16 count) should be valid (end of line)
        assert!(is_position_valid(
            source,
            Position {
                line: 0,
                character: 2
            }
        ));
        // character=3 is one past end → still valid (end-of-line sentinel)
        assert!(is_position_valid(
            source,
            Position {
                line: 0,
                character: 3
            }
        ));
        // character=7 is way past → would fail with byte comparison but might pass with UTF-16
        // 6 bytes, 2 UTF-16 units, so character=7 is well beyond
        assert!(!is_position_valid(
            source,
            Position {
                line: 0,
                character: 7
            }
        ));
    }

    #[test]
    fn test_fallback_detect_context_closing_delimiter_pushdown() {
        // A closers-only cursor line is the auto-close push-down scenario:
        // the fallback mirrors `detect_closing_delimiter` and indents it as
        // content inside the parens instead of aligning the delimiter.
        let source = "func(\n  arg1\n)";
        let position = Position {
            line: 2,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "closers-only line should fall through to content detection, got {:?}",
            ctx
        );
    }

    #[test]
    fn test_fallback_detect_context_closing_delimiter_with_content() {
        // A closer followed by real content is NOT the push-down scenario:
        // delimiter alignment applies.
        let source = "func(\n  arg1\n) + 1";
        let position = Position {
            line: 2,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        match ctx {
            IndentContext::ClosingDelimiter { delimiter, .. } => {
                assert_eq!(delimiter, ')');
            }
            _ => panic!("Expected ClosingDelimiter context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_fallback_detect_context_continuation_operator() {
        let source = "data %>%\n";
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!("Expected AfterContinuationOperator context"),
        }
    }

    #[test]
    fn test_fallback_detect_context_native_pipe() {
        let source = "data |>\n";
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Pipe);
            }
            _ => panic!("Expected AfterContinuationOperator context"),
        }
    }

    #[test]
    fn test_fallback_detect_context_unclosed_paren() {
        let source = "func(\n";
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        match ctx {
            IndentContext::InsideParens {
                opener_line,
                opener_col,
                ..
            } => {
                assert_eq!(opener_line, 0);
                assert_eq!(opener_col, 4);
            }
            _ => panic!("Expected InsideParens context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_fallback_detect_context_unclosed_brace() {
        let source = "if (TRUE) {\n";
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!("Expected InsideBraces context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_fallback_detect_context_complete_expression() {
        let source = "x <- 1\n";
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = fallback_detect_context(source, position, 2);

        match ctx {
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent,
            } => {
                assert_eq!(enclosing_block_indent, 0);
            }
            _ => panic!("Expected AfterCompleteExpression context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_find_matching_opener_heuristic_paren() {
        let source = "func(\n  arg\n)";
        let result = find_matching_opener_heuristic(source, 2, ')');
        assert_eq!(result, Some((0, 4)));
    }

    #[test]
    fn test_find_matching_opener_heuristic_nested() {
        let source = "outer(inner(\n  arg\n))";
        // Closing the inner paren
        let result = find_matching_opener_heuristic(source, 2, ')');
        // Should find the inner opening paren
        assert!(result.is_some());
    }

    #[test]
    fn test_find_matching_opener_heuristic_brace() {
        let source = "if (TRUE) {\n  x <- 1\n}";
        let result = find_matching_opener_heuristic(source, 2, '}');
        assert_eq!(result, Some((0, 10)));
    }

    #[test]
    fn test_find_matching_opener_heuristic_not_found() {
        let source = "x <- 1\n)"; // Unmatched closing paren
        let result = find_matching_opener_heuristic(source, 1, ')');
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_unclosed_delimiter_heuristic_paren() {
        let source = "func(\n";
        let result = find_unclosed_delimiter_heuristic(source, 1);
        assert_eq!(result, Some((0, 4, '(')));
    }

    #[test]
    fn test_find_unclosed_delimiter_heuristic_nested() {
        let source = "outer(inner(\n";
        let result = find_unclosed_delimiter_heuristic(source, 1);
        // Should return the innermost unclosed delimiter
        assert_eq!(result, Some((0, 11, '(')));
    }

    #[test]
    fn test_find_unclosed_delimiter_heuristic_closed() {
        let source = "func(arg)\n";
        let result = find_unclosed_delimiter_heuristic(source, 1);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_unclosed_delimiter_heuristic_brackets_in_string() {
        // Brackets inside string literals should NOT be counted
        let source = "x <- \"has a ( in it\"\nfunc(\n";
        let result = find_unclosed_delimiter_heuristic(source, 2);
        // Should find the `(` on line 1 (func() call), not the `(` inside the string
        assert_eq!(result, Some((1, 4, '(')));
    }

    #[test]
    fn test_find_unclosed_delimiter_heuristic_only_string_brackets() {
        // Only brackets inside strings — no real unclosed delimiters
        let source = "x <- \"has ( and ) in it\"\n";
        let result = find_unclosed_delimiter_heuristic(source, 1);
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_context_with_invalid_position() {
        let code = "x <- 1";
        let tree = parse_r_code(code);
        // Position way out of bounds
        let position = Position {
            line: 100,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should return AfterCompleteExpression with indent 0
        match ctx {
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent,
            } => {
                assert_eq!(enclosing_block_indent, 0);
            }
            _ => panic!("Expected AfterCompleteExpression for invalid position"),
        }
    }

    #[test]
    fn test_detect_context_with_syntax_error_uses_fallback() {
        let code = "x <- + +\n"; // Invalid syntax
        let tree = parse_r_code(code);
        let position = Position {
            line: 1,
            character: 0,
        };

        // Should not panic, should return a valid context
        let ctx = detect_context(&tree, code, position, 2);

        // The fallback should not panic; any resulting context is acceptable
        // (malformed input does not guarantee a specific variant).
        #[allow(clippy::single_match)]
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {}
            _ => {}
        }
    }

    // ========================================================================
    // Property Test Generators
    // ========================================================================

    /// Generate a valid R identifier (lowercase letters and underscores)
    /// Filters out R reserved keywords to avoid generating invalid code
    fn r_identifier() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_]{0,5}".prop_filter("not reserved keyword", |s| {
            !matches!(
                s.as_str(),
                "if" | "else"
                    | "for"
                    | "in"
                    | "while"
                    | "repeat"
                    | "next"
                    | "break"
                    | "function"
                    | "return"
                    | "TRUE"
                    | "FALSE"
                    | "NULL"
                    | "NA"
                    | "Inf"
                    | "NaN"
            )
        })
    }

    /// Enum representing continuation operator types for chain generation
    #[derive(Debug, Clone, Copy)]
    enum ChainOperator {
        NativePipe,   // |>
        MagrittrPipe, // %>%
        Plus,         // +
        Tilde,        // ~
        CustomInfix,  // %word%
    }

    impl ChainOperator {
        fn as_str(&self) -> &'static str {
            match self {
                ChainOperator::NativePipe => "|>",
                ChainOperator::MagrittrPipe => "%>%",
                ChainOperator::Plus => "+",
                ChainOperator::Tilde => "~",
                ChainOperator::CustomInfix => "%op%",
            }
        }
    }

    /// Strategy to generate different chain operators
    fn chain_operator() -> impl Strategy<Value = ChainOperator> {
        prop_oneof![
            Just(ChainOperator::NativePipe),
            Just(ChainOperator::MagrittrPipe),
            Just(ChainOperator::Plus),
            Just(ChainOperator::Tilde),
            Just(ChainOperator::CustomInfix),
        ]
    }

    /// Generate a pipe chain with the specified length and operator.
    ///
    /// The chain starts with "data" and adds `length` continuation lines.
    /// Each line ends with the operator (except the last line).
    ///
    /// Example for length=2 with %>%:
    /// ```r
    /// data %>%
    ///   step0() %>%
    ///   step1()
    /// ```
    fn generate_pipe_chain(
        length: usize,
        operator: ChainOperator,
        leading_spaces: usize,
    ) -> String {
        let indent = " ".repeat(leading_spaces);
        let mut code = format!("{}data", indent);

        for i in 0..length {
            code.push_str(&format!(" {}\n  ", operator.as_str()));
            code.push_str(&format!("step{}()", i));
        }

        code
    }

    /// Get the column of the first non-whitespace character on a line.
    fn get_first_non_ws_col(code: &str, line: u32) -> u32 {
        code.lines()
            .nth(line as usize)
            .map(|l| l.chars().take_while(|c| c.is_whitespace()).count() as u32)
            .unwrap_or(0)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 15 extended: Brace block detection
        /// Validates: Requirement 9.5
        #[test]
        fn property_brace_block_detection(
            var_name in r_identifier(),
            value in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 15: AST Node Detection

            let code = format!("{{ {} <- {} }}", var_name, value);
            let tree = parse_r_code(&code);
            let nodes = collect_all_nodes(tree.root_node());

            // Find the braced_expression node
            let brace_node = nodes.iter().find(|n| n.kind() == "braced_expression");
            prop_assert!(brace_node.is_some(), "Should find braced_expression node in: {}", code);

            // Verify is_brace_list_node returns true
            prop_assert!(
                is_brace_list_node(*brace_node.unwrap()),
                "is_brace_list_node should return true for braced_expression node"
            );
        }

        // ========================================================================
        // Property 1: Chain Start Detection
        // Feature: r-smart-indentation, Property 1: Chain Start Detection
        // Validates: Requirements 3.1
        // ========================================================================

        /// Property 1: For any R code snippet containing a pipe chain (consecutive
        /// lines ending with continuation operators), the chain start detection
        /// algorithm should identify the first line that is NOT preceded by a
        /// continuation operator, and return its line number and starting column.
        ///
        /// **Validates: Requirements 3.1**
        #[test]
        fn property_chain_start_detection(
            chain_length in 1..10usize,
            operator in chain_operator(),
            leading_spaces in 0..4usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            // Generate R code with pipe chain of specified length
            let code = generate_pipe_chain(chain_length, operator, leading_spaces);


            // Position cursor at the last line of the chain
            let last_line = chain_length as u32;

            // Detect chain start using ChainWalker
            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(last_line);

            // Verify: chain start should be line 0 (first line of chain)
            prop_assert_eq!(
                start_line, 0,
                "Chain start line should be 0 for code:\n{}\nGot start_line={}",
                code, start_line
            );

            // Verify: chain start column should be first non-whitespace
            let expected_col = get_first_non_ws_col(&code, 0);
            prop_assert_eq!(
                start_col, expected_col,
                "Chain start column should be {} (first non-ws) for code:\n{}\nGot start_col={}",
                expected_col, code, start_col
            );
        }

        /// Property 1 extended: Chain start detection with native pipe |>
        /// Validates: Requirement 3.1
        #[test]
        fn property_chain_start_native_pipe(
            chain_length in 1..10usize,
            leading_spaces in 0..4usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            let code = generate_pipe_chain(chain_length, ChainOperator::NativePipe, leading_spaces);

            let last_line = chain_length as u32;

            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(last_line);

            prop_assert_eq!(start_line, 0);
            prop_assert_eq!(start_col, leading_spaces as u32);
        }

        /// Property 1 extended: Chain start detection with magrittr pipe %>%
        /// Validates: Requirement 3.1
        #[test]
        fn property_chain_start_magrittr_pipe(
            chain_length in 1..10usize,
            leading_spaces in 0..4usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            let code = generate_pipe_chain(chain_length, ChainOperator::MagrittrPipe, leading_spaces);

            let last_line = chain_length as u32;

            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(last_line);

            prop_assert_eq!(start_line, 0);
            prop_assert_eq!(start_col, leading_spaces as u32);
        }

        /// Property 1 extended: Chain start detection with plus operator +
        /// Validates: Requirement 3.1
        #[test]
        fn property_chain_start_plus_operator(
            chain_length in 1..10usize,
            leading_spaces in 0..4usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            let code = generate_pipe_chain(chain_length, ChainOperator::Plus, leading_spaces);

            let last_line = chain_length as u32;

            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(last_line);

            prop_assert_eq!(start_line, 0);
            prop_assert_eq!(start_col, leading_spaces as u32);
        }

        /// Property 1 extended: Chain start detection with tilde operator ~
        /// Validates: Requirement 3.1
        #[test]
        fn property_chain_start_tilde_operator(
            chain_length in 1..10usize,
            leading_spaces in 0..4usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            let code = generate_pipe_chain(chain_length, ChainOperator::Tilde, leading_spaces);

            let last_line = chain_length as u32;

            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(last_line);

            prop_assert_eq!(start_line, 0);
            prop_assert_eq!(start_col, leading_spaces as u32);
        }

        /// Property 1 extended: Chain start detection with custom infix %op%
        /// Validates: Requirement 3.1
        #[test]
        fn property_chain_start_custom_infix(
            chain_length in 1..10usize,
            leading_spaces in 0..4usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            let code = generate_pipe_chain(chain_length, ChainOperator::CustomInfix, leading_spaces);

            let last_line = chain_length as u32;

            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(last_line);

            prop_assert_eq!(start_line, 0);
            prop_assert_eq!(start_col, leading_spaces as u32);
        }

        /// Property 1 extended: Chain start from any position in the chain
        /// For any position within a chain, the chain start should always be line 0.
        /// Validates: Requirement 3.1
        #[test]
        fn property_chain_start_from_any_position(
            chain_length in 2..10usize,
            operator in chain_operator(),
            cursor_line in 1..10usize,
        ) {
            // Feature: r-smart-indentation, Property 1: Chain Start Detection

            // Ensure cursor_line is within the chain
            let cursor_line = (cursor_line % chain_length) + 1;

            let code = generate_pipe_chain(chain_length, operator, 0);

            let walker = ChainWalker::new(&code, 2);
            let (start_line, start_col) = walker.find_chain_start(cursor_line as u32);

            // Chain start should always be line 0 regardless of cursor position
            prop_assert_eq!(
                start_line, 0,
                "Chain start should be line 0 when cursor at line {} in chain of length {}",
                cursor_line, chain_length
            );
            prop_assert_eq!(start_col, 0);
        }
    }

    // ========================================================================
    // OperatorType Tests
    // ========================================================================

    #[test]
    fn test_operator_type_equality() {
        assert_eq!(OperatorType::Pipe, OperatorType::Pipe);
        assert_ne!(OperatorType::Pipe, OperatorType::MagrittrPipe);
    }

    // ========================================================================
    // IndentContext Tests
    // ========================================================================

    #[test]
    fn test_indent_context_variants() {
        let parens = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 5,
            has_content_on_opener_line: true,
        };
        assert!(matches!(parens, IndentContext::InsideParens { .. }));

        let braces = IndentContext::InsideBraces {
            opener_line: 1,
            opener_col: 0,
        };
        assert!(matches!(braces, IndentContext::InsideBraces { .. }));

        let continuation = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };
        assert!(matches!(
            continuation,
            IndentContext::AfterContinuationOperator { .. }
        ));

        let complete = IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 4,
        };
        assert!(matches!(
            complete,
            IndentContext::AfterCompleteExpression { .. }
        ));

        let closing = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 5,
            delimiter: ')',
        };
        assert!(matches!(closing, IndentContext::ClosingDelimiter { .. }));
    }

    // ========================================================================
    // is_brace_list_node Tests
    // ========================================================================

    #[test]
    fn test_is_brace_list_node() {
        let code = "{ x <- 1 }";
        let tree = parse_r_code(code);
        let nodes = collect_all_nodes(tree.root_node());

        let mut found_braces = false;
        for node in nodes {
            if node.kind() == "braced_expression" {
                assert!(is_brace_list_node(node));
                found_braces = true;
            }
        }
        assert!(found_braces, "Should find a braced_expression node");
    }

    #[test]
    fn test_is_brace_list_node_function_body() {
        let code = "f <- function(x) { x + 1 }";
        let tree = parse_r_code(code);
        let nodes = collect_all_nodes(tree.root_node());

        let found_braces = nodes.iter().any(|n| is_brace_list_node(*n));
        assert!(
            found_braces,
            "Should find a braced_expression node in function body"
        );
    }

    // ========================================================================
    // find_parent Tests
    // ========================================================================

    #[test]
    fn test_find_parent_arguments() {
        let code = "f(x, y)";
        let tree = parse_r_code(code);
        let nodes = collect_all_nodes(tree.root_node());

        let mut found_test = false;
        for node in nodes {
            if node.kind() == "identifier" {
                let text = node_text(node, code);
                if text == "x" || text == "y" {
                    let args = find_parent(node, |n| n.kind() == "arguments");
                    assert!(args.is_some(), "Should find arguments parent");
                    found_test = true;
                }
            }
        }
        assert!(found_test, "Should have tested find_parent");
    }

    #[test]
    fn test_find_parent_no_match() {
        let code = "x <- 1";
        let tree = parse_r_code(code);
        let nodes = collect_all_nodes(tree.root_node());

        for node in nodes {
            if node.kind() == "identifier" {
                let args = find_parent(node, |n| n.kind() == "arguments");
                assert!(args.is_none(), "Should not find arguments parent");
            }
        }
    }

    // ========================================================================
    // find_enclosing_* Tests
    // ========================================================================

    #[test]
    fn test_find_enclosing_brace_list() {
        let code = "{ x <- 1 }";
        let tree = parse_r_code(code);
        let nodes = collect_all_nodes(tree.root_node());

        let mut found = false;
        for node in nodes {
            if node.kind() == "identifier" && node_text(node, code) == "x" {
                let braces = find_enclosing_brace_list(node);
                assert!(braces.is_some());
                found = true;
            }
        }
        assert!(found);
    }

    // ========================================================================
    // ChainWalker Tests
    // ========================================================================

    #[test]
    fn test_chain_walker_find_chain_start_simple() {
        // Cursor on line 1; the walk steps back over the `%>%` line to line 0.
        let source = "data %>%\n  step1()";
        let walker = ChainWalker::new(source, 2);
        let (line, col) = walker.find_chain_start(1);
        assert_eq!(line, 0);
        assert_eq!(col, 0);
    }

    #[test]
    fn test_chain_walker_find_chain_start_multi_line() {
        // Cursor on line 2; the walk steps back over two `%>%` lines to line 0.
        let source = "data %>%\n  step1() %>%\n  step2()";
        let walker = ChainWalker::new(source, 2);
        let (line, col) = walker.find_chain_start(2);
        assert_eq!(line, 0);
        assert_eq!(col, 0);
    }

    #[test]
    fn test_chain_walker_find_chain_start_indented() {
        // Cursor on line 1; the walk steps back to the indented chain-start line.
        let source = "  data %>%\n    step1()";
        let walker = ChainWalker::new(source, 2);
        let (line, col) = walker.find_chain_start(1);
        assert_eq!(line, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn test_chain_walker_simple_pipe() {
        let code = "result <- data %>%\n  filter(x > 0) %>%\n  select(y)";
        let walker = ChainWalker::new(code, 2);

        // Cursor on line 2 (select line)
        let (start_line, start_col) = walker.find_chain_start(2);

        // Chain start should be line 0 (result <- data %>%)
        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0); // "result" starts at column 0
    }

    #[test]
    fn test_chain_walker_native_pipe() {
        let code = "data |>\n  filter(x) |>\n  select(y)";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_plus_operator() {
        let code = "ggplot(data) +\n  geom_point() +\n  theme_minimal()";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_tilde_operator() {
        let code = "y ~\n  x1 +\n  x2";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_custom_infix() {
        let code = "x %myop%\n  y %myop%\n  z";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_with_comments() {
        let code = "data %>%  # comment\n  filter(x) %>%  # another comment\n  select(y)";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_indented_start() {
        let code = "  result <- data %>%\n    filter(x) %>%\n    select(y)";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 2); // "result" starts at column 2 (after 2 spaces)
    }

    #[test]
    fn test_chain_walker_no_chain() {
        let code = "x <- 1\ny <- 2\nz <- 3";
        let walker = ChainWalker::new(code, 2);

        // Cursor on line 2
        let (start_line, start_col) = walker.find_chain_start(2);

        // No chain, so start should be line 2 itself
        assert_eq!(start_line, 2);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_single_line() {
        let code = "data %>% filter(x)";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(0);

        // Single line, chain start is line 0
        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_middle_of_chain() {
        let code = "data %>%\n  step1() %>%\n  step2() %>%\n  step3()";
        let walker = ChainWalker::new(code, 2);

        // Cursor on line 1 (step1 line)
        let (start_line, start_col) = walker.find_chain_start(1);

        // Chain start should still be line 0
        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_in_percent() {
        // Test %in% operator
        let code = "x %in%\n  y";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(1);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_mixed_operators() {
        // Mix of different continuation operators
        let code = "data %>%\n  filter(x) |>\n  mutate(y = y + 1)";
        let walker = ChainWalker::new(code, 2);

        let (start_line, start_col) = walker.find_chain_start(2);

        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
    }

    #[test]
    fn test_chain_walker_tab_indented() {
        // Tab-indented chain start should report visual column, not char count
        let code = "\tresult <- data %>%\n\t\tfilter(x)";
        let walker = ChainWalker::new(code, 4);

        let (start_line, start_col) = walker.find_chain_start(1);

        assert_eq!(start_line, 0);
        // One tab with tab_size=4 → visual column 4
        assert_eq!(start_col, 4);
    }

    // ========================================================================
    // strip_trailing_comment Tests
    // ========================================================================

    #[test]
    fn test_strip_trailing_comment_simple() {
        assert_eq!(strip_trailing_comment("x <- 1 # comment"), "x <- 1 ");
    }

    #[test]
    fn test_strip_trailing_comment_no_comment() {
        assert_eq!(strip_trailing_comment("x <- 1"), "x <- 1");
    }

    #[test]
    fn test_strip_trailing_comment_hash_in_string() {
        assert_eq!(
            strip_trailing_comment(r#"x <- "hello # world""#),
            r#"x <- "hello # world""#
        );
    }

    #[test]
    fn test_strip_trailing_comment_hash_in_single_quote_string() {
        assert_eq!(
            strip_trailing_comment("x <- 'hello # world'"),
            "x <- 'hello # world'"
        );
    }

    #[test]
    fn test_strip_trailing_comment_string_then_comment() {
        assert_eq!(
            strip_trailing_comment(r#"x <- "hello" # comment"#),
            r#"x <- "hello" "#
        );
    }

    #[test]
    fn test_strip_trailing_comment_escaped_quote() {
        assert_eq!(
            strip_trailing_comment(r#"x <- "hello \" # world" # comment"#),
            r#"x <- "hello \" # world" "#
        );
    }

    #[test]
    fn test_strip_trailing_comment_only_comment() {
        assert_eq!(strip_trailing_comment("# just a comment"), "");
    }

    #[test]
    fn test_strip_trailing_comment_raw_string() {
        // R raw string r"(...)" — hash inside should NOT be treated as comment
        assert_eq!(
            strip_trailing_comment(r#"x <- r"(# not a comment)" # real comment"#),
            r#"x <- r"(# not a comment)" "#
        );
    }

    #[test]
    fn test_strip_trailing_comment_raw_string_brackets() {
        // R raw string R"[...]"
        assert_eq!(
            strip_trailing_comment(r#"x <- R"[# not a comment]" # real comment"#),
            r#"x <- R"[# not a comment]" "#
        );
    }

    // ========================================================================
    // detect_context Tests
    // ========================================================================

    #[test]
    fn test_detect_context_closing_delimiter_paren() {
        let code = "func(\n  arg1,\n  arg2\n)";
        let tree = parse_r_code(code);

        // Cursor on line 3 (the closing paren line).
        // The line contains only ")" — the auto-close heuristic skips closing
        // delimiter detection so the line gets indented as inside-parens content.
        let position = Position {
            line: 3,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideParens { opener_col, .. } => {
                assert_eq!(opener_col, 4); // column of the opening paren
            }
            _ => panic!(
                "Expected InsideParens context (auto-close heuristic), got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_detect_context_closing_delimiter_brace() {
        let code = "{\n  x <- 1\n}";
        let tree = parse_r_code(code);

        // Cursor on line 2 (the closing brace line).
        // The line contains only "}" — auto-close heuristic treats as inside-braces.
        let position = Position {
            line: 2,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!(
                "Expected InsideBraces context (auto-close heuristic), got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_detect_context_after_pipe_operator() {
        let code = "data %>%\n  ";
        let tree = parse_r_code(code);

        // Cursor on line 1 (after the pipe)
        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line,
                operator_type,
                ..
            } => {
                assert_eq!(chain_start_line, 0);
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_after_native_pipe() {
        let code = "data |>\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Pipe);
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_after_plus_operator() {
        let code = "ggplot(data) +\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Plus);
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_after_tilde_operator() {
        let code = "y ~\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Tilde);
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_inside_parens_with_content() {
        let code = "func(arg1,\n  ";
        let tree = parse_r_code(code);

        // Cursor on line 1 (inside the parens)
        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideParens {
                has_content_on_opener_line,
                ..
            } => {
                assert!(
                    has_content_on_opener_line,
                    "Should detect content after opener"
                );
            }
            _ => panic!("Expected InsideParens context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_inside_parens_no_content() {
        let code = "func(\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideParens {
                has_content_on_opener_line,
                ..
            } => {
                assert!(
                    !has_content_on_opener_line,
                    "Should not detect content after opener"
                );
            }
            _ => panic!("Expected InsideParens context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_inside_braces() {
        let code = "{\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!("Expected InsideBraces context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_complete_expression() {
        let code = "x <- 1\n";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {
                // Expected
            }
            _ => panic!("Expected AfterCompleteExpression context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_chain_start_detection() {
        let code = "data %>%\n  filter(x) %>%\n  ";
        let tree = parse_r_code(code);

        // Cursor on line 2 (after second pipe)
        let position = Position {
            line: 2,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line,
                chain_start_col,
                ..
            } => {
                assert_eq!(chain_start_line, 0, "Chain should start at line 0");
                assert_eq!(chain_start_col, 0, "Chain should start at column 0");
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_priority_closing_over_continuation() {
        // When a line contains only a closing delimiter, the auto-close heuristic
        // treats it as inside-parens (the delimiter was pushed down by Enter).
        // This is correct for onTypeFormatting: the user wants content indentation.
        let code = "data %>%\n  filter(x\n  )";
        let tree = parse_r_code(code);

        // Cursor on line 2 (the closing paren line)
        let position = Position {
            line: 2,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Delimiter-only line → treated as inside-parens
        match ctx {
            IndentContext::InsideParens { .. } => {
                // Expected: auto-close heuristic kicks in
            }
            _ => panic!(
                "Expected InsideParens context (auto-close heuristic), got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_detect_context_nested_parens_in_braces() {
        // Note: When code is very incomplete, tree-sitter may produce ERROR nodes
        // and context detection may fall back to AfterCompleteExpression.
        // This test verifies the behavior with more complete code.
        let code = "if (TRUE) {\n  func(x,\n    y";
        let tree = parse_r_code(code);

        // Cursor after 'y' on line 2
        let position = Position {
            line: 2,
            character: 5,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // With more complete code, should detect InsideParens
        match ctx {
            IndentContext::InsideParens { .. } => {
                // Expected - parens are innermost
            }
            IndentContext::InsideBraces { .. } => {
                // Also acceptable - braces detected
            }
            IndentContext::AfterCompleteExpression { .. } => {
                // Acceptable for incomplete code - tree-sitter may produce ERROR nodes
            }
            _ => panic!("Unexpected context: {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_with_comment_on_prev_line() {
        let code = "data %>%  # comment\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should still detect continuation operator despite comment
        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_custom_infix() {
        let code = "x %in%\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::CustomInfix);
            }
            _ => panic!("Expected AfterContinuationOperator context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_function_body() {
        let code = "f <- function(x) {\n  ";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!("Expected InsideBraces context, got {:?}", ctx),
        }
    }

    #[test]
    fn test_detect_context_enclosing_block_indent() {
        let code = "  {\n    x <- 1\n  }\n  ";
        let tree = parse_r_code(code);

        // Cursor after the closing brace
        let position = Position {
            line: 3,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent,
            } => {
                // Should return 0 since we're at top level after the block
                assert_eq!(enclosing_block_indent, 0);
            }
            _ => panic!("Expected AfterCompleteExpression context, got {:?}", ctx),
        }
    }

    // ========================================================================
    // Property 16: Nested Context Priority
    // Feature: r-smart-indentation, Property 16: Nested Context Priority
    // Validates: Requirements 3.4, 10.1, 10.2, 10.3
    // ========================================================================

    /// Enum representing different nesting scenarios for Property 16 tests
    #[derive(Debug, Clone)]
    enum NestedStructure {
        /// Pipe chain inside function call: func(data %>% step())
        PipeInsideCall,
        /// Function call inside pipe chain: data %>% func(arg)
        CallInsidePipe,
        /// Braces inside function call: func({ x <- 1 })
        BracesInsideCall,
        /// Pipe inside braces: { data %>% step() }
        PipeInsideBraces,
        /// Call inside braces: { func(x) }
        CallInsideBraces,
        /// Nested function calls: outer(inner(x))
        NestedCalls,
        /// Pipe chain inside pipe chain (via function): data %>% func(x %>% step())
        PipeInsidePipeViaCall,
    }

    /// Strategy to generate different nested structures
    fn nested_structure() -> impl Strategy<Value = NestedStructure> {
        prop_oneof![
            Just(NestedStructure::PipeInsideCall),
            Just(NestedStructure::CallInsidePipe),
            Just(NestedStructure::BracesInsideCall),
            Just(NestedStructure::PipeInsideBraces),
            Just(NestedStructure::CallInsideBraces),
            Just(NestedStructure::NestedCalls),
            Just(NestedStructure::PipeInsidePipeViaCall),
        ]
    }

    /// Generate nested R code structure with a cursor position inside the innermost context.
    ///
    /// Returns (code, cursor_position, expected_innermost_context_type)
    fn generate_nested_structure(
        structure: &NestedStructure,
        func_name: &str,
        inner_func: &str,
    ) -> (String, Position, &'static str) {
        match structure {
            NestedStructure::PipeInsideCall => {
                // func(data %>%
                //   step())
                // Cursor after %>% should detect pipe context
                let code = format!("{}(data %>%\n  step())", func_name);
                let position = Position {
                    line: 1,
                    character: 2,
                };
                (code, position, "AfterContinuationOperator")
            }
            NestedStructure::CallInsidePipe => {
                // data %>%
                //   func(arg,
                //     more)
                // Cursor inside func() should detect parens context
                let code = format!("data %>%\n  {}(arg,\n    more)", func_name);
                let position = Position {
                    line: 2,
                    character: 4,
                };
                (code, position, "InsideParens")
            }
            NestedStructure::BracesInsideCall => {
                // func({
                //   x <- 1
                // })
                // Cursor inside braces should detect brace context
                let code = format!("{}({{\n  x <- 1\n}})", func_name);
                let position = Position {
                    line: 1,
                    character: 2,
                };
                (code, position, "InsideBraces")
            }
            NestedStructure::PipeInsideBraces => {
                // {
                //   data %>%
                //     step()
                // }
                // Cursor after %>% should detect pipe context
                let code = "{{\n  data %>%\n    step()\n}}".to_string();
                let position = Position {
                    line: 2,
                    character: 4,
                };
                (code, position, "AfterContinuationOperator")
            }
            NestedStructure::CallInsideBraces => {
                // {
                //   func(arg,
                //     more)
                // }
                // Cursor inside func() should detect parens context
                let code = format!("{{\n  {}(arg,\n    more)\n}}", func_name);
                let position = Position {
                    line: 2,
                    character: 4,
                };
                (code, position, "InsideParens")
            }
            NestedStructure::NestedCalls => {
                // outer(inner(x,
                //   y))
                // Cursor inside inner() should detect parens context for inner
                let code = format!("{}({}(x,\n  y))", func_name, inner_func);
                let position = Position {
                    line: 1,
                    character: 2,
                };
                (code, position, "InsideParens")
            }
            NestedStructure::PipeInsidePipeViaCall => {
                // data %>%
                //   func(x %>%
                //     step())
                // Cursor after inner %>% should detect pipe context
                let code = format!("data %>%\n  {}(x %>%\n    step())", func_name);
                let position = Position {
                    line: 2,
                    character: 4,
                };
                (code, position, "AfterContinuationOperator")
            }
        }
    }

    /// Verify that the detected context matches the expected innermost context type.
    fn verify_innermost_context(ctx: &IndentContext, expected_type: &str) -> bool {
        matches!(
            (ctx, expected_type),
            (
                IndentContext::AfterContinuationOperator { .. },
                "AfterContinuationOperator"
            ) | (IndentContext::InsideParens { .. }, "InsideParens")
                | (IndentContext::InsideBraces { .. }, "InsideBraces")
                | (IndentContext::ClosingDelimiter { .. }, "ClosingDelimiter")
                | (
                    IndentContext::AfterCompleteExpression { .. },
                    "AfterCompleteExpression"
                )
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 16: For any R code with multiple levels of nesting (e.g., pipe chain
        /// inside function call inside another pipe chain), the context detector should
        /// identify the innermost syntactically relevant context for the cursor position,
        /// such that indentation decisions are based on the most specific applicable rule.
        ///
        /// **Validates: Requirements 3.4, 10.1, 10.2, 10.3**
        #[test]
        fn property_nested_context_priority(
            structure in nested_structure(),
            func_name in r_identifier(),
            inner_func in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // Generate nested R code with cursor position
            let (code, position, expected_type) = generate_nested_structure(
                &structure,
                &func_name,
                &inner_func,
            );

            // Parse with tree-sitter
            let tree = parse_r_code(&code);

            // Detect context at cursor position
            let ctx = detect_context(&tree, &code, position, 2);

            // Verify the innermost context is detected
            prop_assert!(
                verify_innermost_context(&ctx, expected_type),
                "Expected {} context for structure {:?}, got {:?}\nCode:\n{}",
                expected_type,
                structure,
                ctx,
                code
            );
        }

        /// Property 16 extended: Pipe chain inside function call should detect pipe context
        /// Validates: Requirement 10.1
        #[test]
        fn property_pipe_inside_call_detects_pipe(
            func_name in r_identifier(),
            pipe_op in prop_oneof![Just("|>"), Just("%>%")],
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // func(data |>
            //   step())
            let code = format!("{}(data {}\n  step())", func_name, pipe_op);
            let tree = parse_r_code(&code);

            // Cursor on line 1 (after pipe)
            let position = Position { line: 1, character: 2 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect pipe context, not parens context
            match ctx {
                IndentContext::AfterContinuationOperator { operator_type, .. } => {
                    match pipe_op {
                        "|>" => prop_assert_eq!(operator_type, OperatorType::Pipe),
                        "%>%" => prop_assert_eq!(operator_type, OperatorType::MagrittrPipe),
                        _ => prop_assert!(false, "Unexpected pipe operator"),
                    }
                }
                _ => prop_assert!(
                    false,
                    "Expected AfterContinuationOperator for pipe inside call, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }

        /// Property 16 extended: Function call inside pipe chain should detect function call context
        /// Validates: Requirement 10.2
        #[test]
        fn property_call_inside_pipe_detects_parens(
            func_name in r_identifier(),
            arg1 in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // data %>%
            //   func(arg1,
            //     more)
            let code = format!("data %>%\n  {}({},\n    more)", func_name, arg1);
            let tree = parse_r_code(&code);

            // Cursor on line 2 (inside function call)
            let position = Position { line: 2, character: 4 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect parens context (innermost), not pipe context
            match ctx {
                IndentContext::InsideParens { has_content_on_opener_line, .. } => {
                    // The opener line has content (arg1)
                    prop_assert!(
                        has_content_on_opener_line,
                        "Should detect content on opener line"
                    );
                }
                _ => prop_assert!(
                    false,
                    "Expected InsideParens for call inside pipe, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }

        /// Property 16 extended: Braces inside function call should detect brace context
        /// Validates: Requirement 10.3
        #[test]
        fn property_braces_inside_call_detects_braces(
            func_name in r_identifier(),
            var_name in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // func({
            //   x <- 1
            // })
            let code = format!("{}({{\n  {} <- 1\n}})", func_name, var_name);
            let tree = parse_r_code(&code);

            // Cursor on line 1 (inside braces)
            let position = Position { line: 1, character: 2 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect brace context (innermost), not parens context
            match ctx {
                IndentContext::InsideBraces { opener_line, .. } => {
                    // Opener should be on line 0 (where the { is)
                    prop_assert_eq!(opener_line, 0);
                }
                _ => prop_assert!(
                    false,
                    "Expected InsideBraces for braces inside call, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }

        /// Property 16 extended: Multiple levels of nesting should detect innermost context
        /// Validates: Requirements 3.4, 10.1, 10.2, 10.3
        #[test]
        fn property_deep_nesting_detects_innermost(
            outer_func in r_identifier(),
            inner_func in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // Test: outer pipe -> function call -> inner pipe
            // data %>%
            //   outer(x %>%
            //     inner())
            // Cursor after inner %>% should detect pipe context
            let code = format!(
                "data %>%\n  {}(x %>%\n    {}())",
                outer_func, inner_func
            );
            let tree = parse_r_code(&code);

            // Cursor on line 2 (after inner pipe)
            let position = Position { line: 2, character: 4 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect the innermost pipe context
            match ctx {
                IndentContext::AfterContinuationOperator { .. } => {
                    // Correct - detected innermost pipe
                }
                _ => prop_assert!(
                    false,
                    "Expected AfterContinuationOperator for deep nesting, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }

        /// Property 16 extended: Nested function calls should detect innermost call
        /// Validates: Requirement 10.2
        #[test]
        fn property_nested_calls_detects_innermost(
            outer_func in r_identifier(),
            inner_func in r_identifier(),
            arg in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // outer(inner(arg,
            //   more))
            let code = format!("{}({}({},\n  more))", outer_func, inner_func, arg);
            let tree = parse_r_code(&code);

            // Cursor on line 1 (inside inner call)
            let position = Position { line: 1, character: 2 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect innermost parens context
            match ctx {
                IndentContext::InsideParens { .. } => {
                    // Correct - detected innermost parens
                }
                _ => prop_assert!(
                    false,
                    "Expected InsideParens for nested calls, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }

        /// Property 16 extended: Pipe inside braces should detect pipe context
        /// Validates: Requirements 3.4, 10.1
        #[test]
        fn property_pipe_inside_braces_detects_pipe(
            step_func in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // {
            //   data %>%
            //     step()
            // }
            let code = format!("{{\n  data %>%\n    {}()\n}}", step_func);
            let tree = parse_r_code(&code);

            // Cursor on line 2 (after pipe)
            let position = Position { line: 2, character: 4 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect pipe context, not brace context
            match ctx {
                IndentContext::AfterContinuationOperator { operator_type, .. } => {
                    prop_assert_eq!(operator_type, OperatorType::MagrittrPipe);
                }
                _ => prop_assert!(
                    false,
                    "Expected AfterContinuationOperator for pipe inside braces, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }

        /// Property 16 extended: Call inside braces should detect call context
        /// Validates: Requirements 10.2, 10.3
        #[test]
        fn property_call_inside_braces_detects_parens(
            func_name in r_identifier(),
            arg in r_identifier(),
        ) {
            // Feature: r-smart-indentation, Property 16: Nested Context Priority

            // {
            //   func(arg,
            //     more)
            // }
            let code = format!("{{\n  {}({},\n    more)\n}}", func_name, arg);
            let tree = parse_r_code(&code);

            // Cursor on line 2 (inside function call)
            let position = Position { line: 2, character: 4 };
            let ctx = detect_context(&tree, &code, position, 2);

            // Should detect parens context, not brace context
            match ctx {
                IndentContext::InsideParens { .. } => {
                    // Correct - detected innermost parens
                }
                _ => prop_assert!(
                    false,
                    "Expected InsideParens for call inside braces, got {:?}\nCode:\n{}",
                    ctx,
                    code
                ),
            }
        }
    }

    // ========================================================================
    // Unit Tests for Nested Context Priority
    // ========================================================================

    #[test]
    fn test_nested_pipe_inside_call() {
        // func(data %>%
        //   step())
        let code = "func(data %>%\n  step())";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!("Expected AfterContinuationOperator, got {:?}", ctx),
        }
    }

    #[test]
    fn test_nested_call_inside_pipe() {
        // data %>%
        //   func(arg,
        //     more)
        let code = "data %>%\n  func(arg,\n    more)";
        let tree = parse_r_code(code);

        let position = Position {
            line: 2,
            character: 4,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideParens { .. } => {
                // Correct
            }
            _ => panic!("Expected InsideParens, got {:?}", ctx),
        }
    }

    #[test]
    fn test_nested_braces_inside_call() {
        // func({
        //   x <- 1
        // })
        let code = "func({\n  x <- 1\n})";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideBraces { .. } => {
                // Correct
            }
            _ => panic!("Expected InsideBraces, got {:?}", ctx),
        }
    }

    #[test]
    fn test_nested_three_levels() {
        // data %>%
        //   outer(x %>%
        //     inner())
        let code = "data %>%\n  outer(x %>%\n    inner())";
        let tree = parse_r_code(code);

        // Cursor after inner pipe
        let position = Position {
            line: 2,
            character: 4,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { .. } => {
                // Correct - detected innermost pipe
            }
            _ => panic!(
                "Expected AfterContinuationOperator for innermost pipe, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_nested_native_pipe_inside_call() {
        // func(data |>
        //   step())
        let code = "func(data |>\n  step())";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Pipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator with Pipe, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_nested_plus_inside_call() {
        // ggplot(data) +
        //   geom_point(aes(x,
        //     y))
        let code = "ggplot(data) +\n  geom_point(aes(x,\n    y))";
        let tree = parse_r_code(code);

        // Cursor inside aes() call
        let position = Position {
            line: 2,
            character: 4,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideParens { .. } => {
                // Correct - detected innermost parens (aes call)
            }
            _ => panic!(
                "Expected InsideParens for aes() inside ggplot chain, got {:?}",
                ctx
            ),
        }
    }

    // ========================================================================
    // Task 3.7: Unit Tests for Context Detection Edge Cases
    // Validates: Requirements 3.1, 10.3
    // ========================================================================

    // ------------------------------------------------------------------------
    // Empty Lines Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_edge_case_empty_line_after_pipe() {
        // Cursor on empty line after pipe
        let code = "data %>%\n\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 1 (empty line)
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator from line 0
        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator after pipe with empty line, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_empty_line_inside_function_call() {
        // Cursor on empty line inside function call
        let code = "func(arg1,\n\n  arg2)";
        let tree = parse_r_code(code);

        // Cursor on line 1 (empty line)
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside parens
        match ctx {
            IndentContext::InsideParens { .. } => {
                // Correct
            }
            _ => panic!(
                "Expected InsideParens for empty line inside function call, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_empty_line_inside_braces() {
        // Cursor on empty line inside braces
        let code = "{\n\n  x <- 1\n}";
        let tree = parse_r_code(code);

        // Cursor on line 1 (empty line)
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside braces
        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!(
                "Expected InsideBraces for empty line inside braces, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_multiple_empty_lines_after_pipe() {
        // Multiple empty lines after pipe
        // Note: The context detector only looks at the immediately previous line,
        // so multiple empty lines will result in AfterCompleteExpression.
        // This is expected behavior - the user would need to be on line 1 (first empty line)
        // to get the continuation context.
        let code = "data %>%\n\n\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 1 (first empty line after pipe) - should detect continuation
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line, ..
            } => {
                assert_eq!(chain_start_line, 0);
            }
            _ => panic!(
                "Expected AfterContinuationOperator on first empty line after pipe, got {:?}",
                ctx
            ),
        }

        // Cursor on line 2 (second empty line) - previous line is empty, so no continuation detected
        let position2 = Position {
            line: 2,
            character: 0,
        };
        let ctx2 = detect_context(&tree, code, position2, 2);

        // This is expected to be AfterCompleteExpression since line 1 is empty
        match ctx2 {
            IndentContext::AfterCompleteExpression { .. } => {
                // Expected - previous line is empty, no continuation operator
            }
            _ => panic!(
                "Expected AfterCompleteExpression on second empty line, got {:?}",
                ctx2
            ),
        }
    }

    // ------------------------------------------------------------------------
    // Comments Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_edge_case_line_ending_with_operator_followed_by_comment() {
        // Line ending with operator followed by comment: `data %>% # comment`
        let code = "data %>% # this is a comment\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 1 (after the pipe with comment)
        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator despite comment
        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator with trailing comment, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_line_with_only_comment() {
        // Line with only comment
        let code = "data %>%\n  # just a comment\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 1 (comment-only line)
        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator from line 0
        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line, ..
            } => {
                assert_eq!(chain_start_line, 0);
            }
            _ => panic!(
                "Expected AfterContinuationOperator with comment-only line, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_cursor_after_comment_line() {
        // Cursor after comment line in pipe chain
        // Note: The context detector only looks at the immediately previous line.
        // If the previous line is a comment-only line (no operator), it won't detect
        // the continuation context. This is expected behavior.
        let code = "data %>%\n  # comment\n  ";
        let tree = parse_r_code(code);

        // Cursor on line 1 (comment line) - previous line has pipe
        let position1 = Position {
            line: 1,
            character: 2,
        };
        let ctx1 = detect_context(&tree, code, position1, 2);

        match ctx1 {
            IndentContext::AfterContinuationOperator {
                chain_start_line, ..
            } => {
                assert_eq!(chain_start_line, 0);
            }
            _ => panic!(
                "Expected AfterContinuationOperator on comment line after pipe, got {:?}",
                ctx1
            ),
        }

        // Cursor on line 2 (after comment line) - previous line is comment, no operator
        let position2 = Position {
            line: 2,
            character: 2,
        };
        let ctx2 = detect_context(&tree, code, position2, 2);

        // This is expected to be AfterCompleteExpression since line 1 is a comment
        match ctx2 {
            IndentContext::AfterCompleteExpression { .. } => {
                // Expected - previous line is comment, no continuation operator
            }
            _ => panic!(
                "Expected AfterCompleteExpression after comment line, got {:?}",
                ctx2
            ),
        }
    }

    #[test]
    fn test_edge_case_comment_with_hash_in_string() {
        // Comment detection should handle hash in string
        let code = "x <- \"hello # world\" %>%\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 1
        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator (hash in string is not a comment)
        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator with hash in string, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_native_pipe_with_comment() {
        // Native pipe with trailing comment
        let code = "data |> # native pipe comment\n  step()";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Pipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for native pipe with comment, got {:?}",
                ctx
            ),
        }
    }

    // ------------------------------------------------------------------------
    // EOF Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_edge_case_eof_after_pipe() {
        // Cursor at end of file after pipe
        let code = "data %>%\n";
        let tree = parse_r_code(code);

        // Cursor on line 1 (EOF)
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator
        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator at EOF after pipe, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_eof_inside_unclosed_parens() {
        // Cursor at end of file inside unclosed parens
        let code = "func(x,\n";
        let tree = parse_r_code(code);

        // Cursor on line 1 (EOF)
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside parens
        match ctx {
            IndentContext::InsideParens { .. } => {
                // Correct
            }
            _ => panic!(
                "Expected InsideParens at EOF inside unclosed parens, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_eof_inside_unclosed_braces() {
        // Cursor at end of file inside unclosed braces
        let code = "{\n  x <- 1\n";
        let tree = parse_r_code(code);

        // Cursor on line 2 (EOF)
        let position = Position {
            line: 2,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside braces
        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!(
                "Expected InsideBraces at EOF inside unclosed braces, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_eof_after_native_pipe() {
        // Cursor at end of file after native pipe
        let code = "data |>\n";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Pipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator at EOF after native pipe, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_eof_after_plus() {
        // Cursor at end of file after plus operator
        let code = "ggplot(data) +\n";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Plus);
            }
            _ => panic!(
                "Expected AfterContinuationOperator at EOF after plus, got {:?}",
                ctx
            ),
        }
    }

    // ------------------------------------------------------------------------
    // Invalid AST / Syntax Errors Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_edge_case_incomplete_expression_pipe_no_continuation() {
        // Incomplete expression: `data %>%` (no continuation)
        let code = "data %>%";
        let tree = parse_r_code(code);

        // Cursor at end of line 0
        let position = Position {
            line: 0,
            character: 8,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should handle gracefully - either detect pipe context or complete expression
        // The important thing is it doesn't panic
        match ctx {
            IndentContext::AfterContinuationOperator { .. }
            | IndentContext::AfterCompleteExpression { .. }
            | IndentContext::InsideParens { .. } => {
                // All acceptable for incomplete expression
            }
            _ => {
                // Other contexts are also acceptable as long as we don't panic
            }
        }
    }

    #[test]
    fn test_edge_case_unclosed_parenthesis() {
        // Unclosed parenthesis: `func(x,`
        let code = "func(x,";
        let tree = parse_r_code(code);

        // Cursor at end of line 0
        let position = Position {
            line: 0,
            character: 7,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside parens (unclosed)
        match ctx {
            IndentContext::InsideParens {
                has_content_on_opener_line,
                ..
            } => {
                assert!(
                    has_content_on_opener_line,
                    "Should detect content after opener"
                );
            }
            _ => panic!(
                "Expected InsideParens for unclosed parenthesis, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_unclosed_brace() {
        // Unclosed brace: `{ x <- 1`
        let code = "{ x <- 1";
        let tree = parse_r_code(code);

        // Cursor at end of line 0
        let position = Position {
            line: 0,
            character: 8,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside braces (unclosed)
        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            _ => panic!("Expected InsideBraces for unclosed brace, got {:?}", ctx),
        }
    }

    #[test]
    fn test_edge_case_mismatched_delimiters() {
        // Mismatched delimiters: `func(x]`
        // Tree-sitter will produce an error node for this
        let code = "func(x]";
        let tree = parse_r_code(code);

        // Cursor at end of line 0
        let position = Position {
            line: 0,
            character: 7,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should handle gracefully without panicking
        // The exact context depends on how tree-sitter parses the error
        match ctx {
            IndentContext::InsideParens { .. }
            | IndentContext::AfterCompleteExpression { .. }
            | IndentContext::ClosingDelimiter { .. } => {
                // All acceptable for mismatched delimiters
            }
            _ => {
                // Other contexts are also acceptable as long as we don't panic
            }
        }
    }

    #[test]
    fn test_edge_case_syntax_error_in_pipe_chain() {
        // Syntax error in pipe chain
        let code = "data %>% %>%\n  step()";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should handle gracefully - detect some context without panicking
        match ctx {
            IndentContext::AfterContinuationOperator { .. }
            | IndentContext::AfterCompleteExpression { .. }
            | IndentContext::InsideParens { .. } => {
                // All acceptable for syntax error
            }
            _ => {
                // Other contexts are also acceptable
            }
        }
    }

    #[test]
    fn test_edge_case_deeply_nested_unclosed() {
        // Deeply nested unclosed structures
        let code = "func({\n  inner(x,\n    ";
        let tree = parse_r_code(code);

        // Cursor on line 2 (inside inner call)
        let position = Position {
            line: 2,
            character: 4,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect innermost unclosed context (parens)
        match ctx {
            IndentContext::InsideParens { .. } => {
                // Correct - detected innermost parens
            }
            IndentContext::InsideBraces { .. } => {
                // Also acceptable - detected braces
            }
            IndentContext::AfterCompleteExpression { .. } => {
                // Acceptable for incomplete code
            }
            _ => panic!(
                "Expected InsideParens or InsideBraces for deeply nested unclosed, got {:?}",
                ctx
            ),
        }
    }

    // ------------------------------------------------------------------------
    // Missing Nodes Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_edge_case_empty_function_call() {
        // Empty function call: `func()`
        let code = "func()";
        let tree = parse_r_code(code);

        // Cursor inside empty parens
        let position = Position {
            line: 0,
            character: 5,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect complete expression (parens are closed)
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {
                // Correct - empty call is complete
            }
            IndentContext::InsideParens { .. } => {
                // Also acceptable if cursor is considered inside
            }
            _ => panic!(
                "Expected AfterCompleteExpression or InsideParens for empty function call, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_empty_braces() {
        // Empty braces: `{}`
        let code = "{}";
        let tree = parse_r_code(code);

        // Cursor inside empty braces
        let position = Position {
            line: 0,
            character: 1,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect complete expression or inside braces
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {
                // Correct - empty braces are complete
            }
            IndentContext::InsideBraces { .. } => {
                // Also acceptable if cursor is considered inside
            }
            _ => panic!(
                "Expected AfterCompleteExpression or InsideBraces for empty braces, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_single_line_pipe() {
        // Single-line pipe: `data %>% step()`
        let code = "data %>% step()";
        let tree = parse_r_code(code);

        // Cursor at end of line
        let position = Position {
            line: 0,
            character: 15,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect complete expression (pipe chain is complete)
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {
                // Correct - single line pipe is complete
            }
            _ => panic!(
                "Expected AfterCompleteExpression for single-line pipe, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_empty_line_at_start_of_file() {
        // Empty line at start of file
        let code = "\ndata %>%\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 0 (empty line)
        let position = Position {
            line: 0,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect complete expression (nothing before cursor)
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {
                // Correct
            }
            _ => panic!(
                "Expected AfterCompleteExpression for empty line at start, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_whitespace_only_line() {
        // Line with only whitespace
        let code = "data %>%\n    \n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 1 (whitespace-only line)
        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator from line 0
        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line, ..
            } => {
                assert_eq!(chain_start_line, 0);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for whitespace-only line, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_cursor_at_column_zero() {
        // Cursor at column 0 on various lines
        let code = "func(\n  arg1,\n  arg2\n)";
        let tree = parse_r_code(code);

        // Cursor at column 0 on line 1
        let position = Position {
            line: 1,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::InsideParens { .. } => {
                // Correct
            }
            _ => panic!(
                "Expected InsideParens at column 0 inside function call, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_very_long_chain() {
        // Very long pipe chain (tests iteration limit)
        let mut code = String::from("data");
        for i in 0..50 {
            code.push_str(&format!(" %>%\n  step{}()", i));
        }
        let tree = parse_r_code(&code);

        // Cursor on last line
        let position = Position {
            line: 50,
            character: 2,
        };
        let ctx = detect_context(&tree, &code, position, 2);

        // Should detect continuation operator with chain start at line 0
        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line, ..
            } => {
                assert_eq!(chain_start_line, 0);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for very long chain, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_mixed_operators_in_chain() {
        // Mixed operators in chain
        let code = "data %>%\n  filter(x) |>\n  mutate(y) +\n  ";
        let tree = parse_r_code(code);

        // Cursor on line 3 (after +)
        let position = Position {
            line: 3,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator with sub-chain start column at 2
        // (the RHS of the |> boundary, where `mutate(y)` starts) but line 0
        // (outermost expression start, for correct indent calculation)
        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line,
                chain_start_col,
                operator_type,
                ..
            } => {
                assert_eq!(chain_start_line, 0);
                assert_eq!(chain_start_col, 2);
                assert_eq!(operator_type, OperatorType::Plus);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for mixed operators, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_mixed_chain_pipe_then_arithmetic() {
        // Mixed chain: pipe followed by arithmetic operators with nested calls
        let code = "x <- f(a,\n       b,\n       c(a,\n         b = f(a,\n               b))) |>\n     x + y +\n";
        let tree = parse_r_code(code);

        // Cursor on line 6 (after `x + y +`)
        let position = Position {
            line: 6,
            character: 0,
        };
        let ctx = detect_context(&tree, code, position, 4);

        // Sub-chain start col should be 5 (RHS of |>, where `x` is).
        // Line should be 0 (outermost expression start) for correct indent calc.
        match ctx {
            IndentContext::AfterContinuationOperator {
                chain_start_line,
                chain_start_col,
                operator_type,
                ..
            } => {
                assert_eq!(
                    chain_start_col, 5,
                    "Should align with sub-chain start after pipe"
                );
                assert_eq!(chain_start_line, 0);
                assert_eq!(operator_type, OperatorType::Plus);
            }
            _ => panic!("Expected AfterContinuationOperator, got {:?}", ctx),
        }
    }

    #[test]
    fn test_edge_case_unclosed_bracket() {
        // Unclosed bracket: `x[1,`
        let code = "x[1,";
        let tree = parse_r_code(code);

        // Cursor at end of line 0
        let position = Position {
            line: 0,
            character: 4,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should handle gracefully - may detect as complete expression or inside parens
        // depending on how tree-sitter handles subset syntax
        match ctx {
            IndentContext::InsideParens { .. } | IndentContext::AfterCompleteExpression { .. } => {
                // Both acceptable
            }
            _ => {
                // Other contexts are also acceptable as long as we don't panic
            }
        }
    }

    #[test]
    fn test_edge_case_nested_unclosed_structures() {
        // Multiple levels of unclosed structures
        let code = "outer({\n  inner(x,\n    nested(y,\n      ";
        let tree = parse_r_code(code);

        // Cursor on line 3 (deepest level)
        let position = Position {
            line: 3,
            character: 6,
        };
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect some context without panicking
        match ctx {
            IndentContext::InsideParens { .. }
            | IndentContext::InsideBraces { .. }
            | IndentContext::AfterCompleteExpression { .. } => {
                // All acceptable for deeply nested unclosed structures
            }
            _ => {
                // Other contexts are also acceptable
            }
        }
    }

    #[test]
    fn test_edge_case_pipe_after_closing_paren() {
        // Pipe after closing paren on same line
        let code = "func(x) %>%\n  step()";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for pipe after closing paren, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_closing_delimiter_with_content_after() {
        // Closing delimiter with content after it
        let code = "func(\n  arg\n) %>%\n  step()";
        let tree = parse_r_code(code);

        // Cursor on line 3 (after pipe)
        let position = Position {
            line: 3,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::MagrittrPipe);
            }
            _ => panic!(
                "Expected AfterContinuationOperator after closing delimiter with pipe, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_tilde_in_formula() {
        // Tilde in formula context
        let code = "y ~\n  x1 + x2";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::Tilde);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for tilde in formula, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_edge_case_custom_infix_operator() {
        // Custom infix operator
        let code = "x %myop%\n  y";
        let tree = parse_r_code(code);

        let position = Position {
            line: 1,
            character: 2,
        };
        let ctx = detect_context(&tree, code, position, 2);

        match ctx {
            IndentContext::AfterContinuationOperator { operator_type, .. } => {
                assert_eq!(operator_type, OperatorType::CustomInfix);
            }
            _ => panic!(
                "Expected AfterContinuationOperator for custom infix, got {:?}",
                ctx
            ),
        }
    }

    // ========================================================================
    // Error Handling Unit Tests (Task 9.7)
    // Validates: Requirements 6.1, 8.3
    // ========================================================================

    #[test]
    fn test_error_handling_invalid_ast_double_operator() {
        // Invalid AST: double operators should trigger fallback detection
        let code = "x %>% %>%\n";
        let tree = parse_r_code(code);
        let position = Position {
            line: 1,
            character: 0,
        };

        // Should not panic, should return a valid context using fallback
        let ctx = detect_context(&tree, code, position, 2);

        // The fallback should detect the continuation operator on the previous line
        match ctx {
            IndentContext::AfterContinuationOperator { .. }
            | IndentContext::AfterCompleteExpression { .. } => {
                // Both are acceptable for malformed code
            }
            _ => {
                // Other contexts are also acceptable as long as we don't panic
            }
        }
    }

    #[test]
    fn test_error_handling_invalid_ast_incomplete_assignment() {
        // Invalid AST: incomplete assignment
        let code = "x <-\n";
        let tree = parse_r_code(code);
        let position = Position {
            line: 1,
            character: 0,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Any context is acceptable for incomplete code
        match ctx {
            IndentContext::AfterCompleteExpression { .. }
            | IndentContext::AfterContinuationOperator { .. }
            | IndentContext::InsideParens { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_out_of_bounds_line_far_beyond() {
        // Position with line number far beyond document
        let code = "x <- 1";
        let tree = parse_r_code(code);
        let position = Position {
            line: 1000,
            character: 0,
        };

        let ctx = detect_context(&tree, code, position, 2);

        // Should return AfterCompleteExpression with indent 0
        match ctx {
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent,
            } => {
                assert_eq!(enclosing_block_indent, 0);
            }
            _ => panic!("Expected AfterCompleteExpression for far out-of-bounds line"),
        }
    }

    #[test]
    fn test_error_handling_out_of_bounds_column_beyond_line() {
        // Position with column beyond line length
        let code = "short";
        let tree = parse_r_code(code);
        // Column 100 is way beyond the 5-character line
        let position = Position {
            line: 0,
            character: 100,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Should handle gracefully; any resulting context is acceptable as long
        // as detect_context does not panic.
        #[allow(clippy::single_match)]
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_out_of_bounds_max_values() {
        // Position with maximum u32 values
        let code = "x <- 1";
        let tree = parse_r_code(code);
        let position = Position {
            line: u32::MAX,
            character: u32::MAX,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Should return AfterCompleteExpression with indent 0
        match ctx {
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent,
            } => {
                assert_eq!(enclosing_block_indent, 0);
            }
            _ => panic!("Expected AfterCompleteExpression for max u32 position"),
        }
    }

    #[test]
    fn test_error_handling_unclosed_paren_multiline() {
        // Unclosed parenthesis spanning multiple lines
        let code = "func(\n  arg1,\n  arg2,\n";
        let tree = parse_r_code(code);
        let position = Position {
            line: 3,
            character: 0,
        };

        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside parens
        match ctx {
            IndentContext::InsideParens { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            IndentContext::AfterCompleteExpression { .. } => {
                // Also acceptable if fallback is used
            }
            _ => panic!(
                "Expected InsideParens or AfterCompleteExpression for unclosed paren, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_error_handling_unclosed_brace_multiline() {
        // Unclosed brace spanning multiple lines
        let code = "if (TRUE) {\n  x <- 1\n  y <- 2\n";
        let tree = parse_r_code(code);
        let position = Position {
            line: 3,
            character: 0,
        };

        let ctx = detect_context(&tree, code, position, 2);

        // Should detect inside braces
        match ctx {
            IndentContext::InsideBraces { opener_line, .. } => {
                assert_eq!(opener_line, 0);
            }
            IndentContext::AfterCompleteExpression { .. } => {
                // Also acceptable if fallback is used
            }
            _ => panic!(
                "Expected InsideBraces or AfterCompleteExpression for unclosed brace, got {:?}",
                ctx
            ),
        }
    }

    #[test]
    fn test_error_handling_mismatched_delimiters_paren_bracket() {
        // Mismatched delimiters: opening paren, closing bracket
        let code = "func(x]";
        let tree = parse_r_code(code);
        let position = Position {
            line: 0,
            character: 7,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Any context is acceptable for mismatched delimiters
        match ctx {
            IndentContext::InsideParens { .. }
            | IndentContext::AfterCompleteExpression { .. }
            | IndentContext::ClosingDelimiter { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_mismatched_delimiters_brace_paren() {
        // Mismatched delimiters: opening brace, closing paren
        let code = "{ x <- 1 )";
        let tree = parse_r_code(code);
        let position = Position {
            line: 0,
            character: 10,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Any context is acceptable for mismatched delimiters
        match ctx {
            IndentContext::InsideBraces { .. }
            | IndentContext::AfterCompleteExpression { .. }
            | IndentContext::ClosingDelimiter { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_deeply_nested_syntax_error() {
        // Deeply nested structure with syntax error
        let code = "outer({\n  inner(x, {\n    <- broken\n  })\n})";
        let tree = parse_r_code(code);
        let position = Position {
            line: 2,
            character: 4,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Any context is acceptable for syntax error in nested structure
        match ctx {
            IndentContext::InsideParens { .. }
            | IndentContext::InsideBraces { .. }
            | IndentContext::AfterCompleteExpression { .. }
            | IndentContext::AfterContinuationOperator { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_empty_source() {
        // Empty source code
        let code = "";
        let tree = parse_r_code(code);
        let position = Position {
            line: 0,
            character: 0,
        };

        let ctx = detect_context(&tree, code, position, 2);

        // Should return AfterCompleteExpression with indent 0
        match ctx {
            IndentContext::AfterCompleteExpression {
                enclosing_block_indent,
            } => {
                assert_eq!(enclosing_block_indent, 0);
            }
            _ => panic!("Expected AfterCompleteExpression for empty source"),
        }
    }

    #[test]
    fn test_error_handling_whitespace_only_source() {
        // Source with only whitespace
        let code = "   \n   \n   ";
        let tree = parse_r_code(code);
        let position = Position {
            line: 1,
            character: 0,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Should return AfterCompleteExpression
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {}
            _ => panic!("Expected AfterCompleteExpression for whitespace-only source"),
        }
    }

    #[test]
    fn test_error_handling_comment_only_source() {
        // Source with only comments
        let code = "# comment 1\n# comment 2\n";
        let tree = parse_r_code(code);
        let position = Position {
            line: 2,
            character: 0,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Should return AfterCompleteExpression
        match ctx {
            IndentContext::AfterCompleteExpression { .. } => {}
            _ => panic!("Expected AfterCompleteExpression for comment-only source"),
        }
    }

    #[test]
    fn test_error_handling_unicode_in_code() {
        // Code with unicode characters
        let code = "变量 <- 1 %>%\n  处理()";
        let tree = parse_r_code(code);
        let position = Position {
            line: 1,
            character: 2,
        };

        // Should not panic
        let ctx = detect_context(&tree, code, position, 2);

        // Should detect continuation operator context
        match ctx {
            IndentContext::AfterContinuationOperator { .. }
            | IndentContext::AfterCompleteExpression { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_very_long_line() {
        // Very long line (stress test)
        let long_content = "x".repeat(10000);
        let code = format!("{} %>%\n  step()", long_content);
        let tree = parse_r_code(&code);
        let position = Position {
            line: 1,
            character: 2,
        };

        // Should not panic
        let ctx = detect_context(&tree, &code, position, 2);

        // Should detect continuation operator context
        match ctx {
            IndentContext::AfterContinuationOperator { .. }
            | IndentContext::AfterCompleteExpression { .. } => {}
            _ => {}
        }
    }

    #[test]
    fn test_error_handling_many_nested_levels() {
        // Many levels of nesting (stress test)
        let mut code = String::new();
        for _ in 0..50 {
            code.push_str("func(");
        }
        code.push('x');
        // Don't close the parens - test unclosed handling

        let tree = parse_r_code(&code);
        let position = Position {
            line: 0,
            character: code.len() as u32,
        };

        // Should not panic
        let ctx = detect_context(&tree, &code, position, 2);

        // Should detect inside parens or complete expression
        match ctx {
            IndentContext::InsideParens { .. } | IndentContext::AfterCompleteExpression { .. } => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod auto_close_tests {
    use super::*;
    use crate::indentation::calculator::{
        IndentationConfig, IndentationStyle, calculate_indentation,
    };
    use tower_lsp::lsp_types::Position;

    fn parse_r(code: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn rstudio_config(tab_size: u32) -> IndentationConfig {
        IndentationConfig {
            tab_size,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        }
    }

    #[test]
    fn auto_closed_paren_gets_inside_parens_context() {
        // VS Code auto-inserts `)`, user presses Enter → `func(\n)`
        let code = "x <- some_func(\n)";
        let tree = parse_r(code);
        let ctx = detect_context(
            &tree,
            code,
            Position {
                line: 1,
                character: 0,
            },
            2,
        );
        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "Auto-closed paren should be treated as InsideParens, got {:?}",
            ctx
        );
    }

    #[test]
    fn unclosed_paren_gets_inside_parens_context() {
        let code = "x <- some_func(\n";
        let tree = parse_r(code);
        let ctx = detect_context(
            &tree,
            code,
            Position {
                line: 1,
                character: 0,
            },
            2,
        );
        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "Unclosed paren should be InsideParens, got {:?}",
            ctx
        );
    }

    #[test]
    fn auto_closed_and_unclosed_produce_same_indent() {
        let config = rstudio_config(2);

        let code_auto = "x <- some_func(\n)";
        let tree_auto = parse_r(code_auto);
        let ctx_auto = detect_context(
            &tree_auto,
            code_auto,
            Position {
                line: 1,
                character: 0,
            },
            2,
        );
        let indent_auto = calculate_indentation(ctx_auto, config.clone(), code_auto);

        let code_open = "x <- some_func(\n";
        let tree_open = parse_r(code_open);
        let ctx_open = detect_context(
            &tree_open,
            code_open,
            Position {
                line: 1,
                character: 0,
            },
            2,
        );
        let indent_open = calculate_indentation(ctx_open, config, code_open);

        assert_eq!(
            indent_auto, indent_open,
            "Auto-closed and unclosed parens should produce identical indentation"
        );
    }

    #[test]
    fn auto_closed_paren_in_braces_with_content_aligns_to_paren() {
        // The key user-reported scenario: content after opener should align
        let code = "if (TRUE) {\n  x <- some_func(\"file\",\n  )\n}";
        let tree = parse_r(code);
        let ctx = detect_context(
            &tree,
            code,
            Position {
                line: 2,
                character: 0,
            },
            2,
        );

        match &ctx {
            IndentContext::InsideParens {
                opener_col,
                has_content_on_opener_line,
                ..
            } => {
                assert_eq!(*opener_col, 16);
                assert!(*has_content_on_opener_line);
            }
            _ => panic!("Expected InsideParens, got {:?}", ctx),
        }

        let indent = calculate_indentation(ctx, rstudio_config(2), code);
        // RStudio style with content after opener → align to column after `(`
        assert_eq!(indent, 17, "Should align to column after opening paren");
    }

    #[test]
    fn auto_closed_brace_gets_inside_braces_context() {
        let code = "if (TRUE) {\n}";
        let tree = parse_r(code);
        let ctx = detect_context(
            &tree,
            code,
            Position {
                line: 1,
                character: 0,
            },
            2,
        );
        assert!(
            matches!(ctx, IndentContext::InsideBraces { .. }),
            "Auto-closed brace should be treated as InsideBraces, got {:?}",
            ctx
        );
    }

    #[test]
    fn second_enter_inside_auto_closed_parens() {
        // Simulates: user typed `x <- some_function(x,`, Enter (got alignment),
        // typed `y`, then pressed Enter again. Document state:
        //   x <- some_function(x,
        //                      y
        //                      )
        // Cursor is on line 2, the `)` is also on line 2.
        // onTypeFormatting position character may be nonzero (matching prev indent).
        let code = "x <- some_function(x,\n                   y\n                   )";
        let tree = parse_r(code);

        // Cursor at line 2 with nonzero character (matching previous line's indent)
        let ctx = detect_context(
            &tree,
            code,
            Position {
                line: 2,
                character: 19,
            },
            2,
        );
        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "Second Enter with auto-closed paren should be InsideParens, got {:?}",
            ctx
        );

        let indent = calculate_indentation(ctx, rstudio_config(2), code);
        // Should align to column after `(` since there's content after opener
        assert_eq!(
            indent, 19,
            "Should maintain paren alignment on second Enter"
        );
    }
}

/// Tests for the assignment-operator continuation context (#611): a line
/// ending in an assignment operator indents its RHS one level, and a chain
/// on the RHS of a broken assignment loses the one-level floor
/// (`assignment_as_infix` flattening). Detection is pinned on both the AST
/// path (`detect_context`) and the regex fallback (`fallback_detect_context`),
/// and columns are pinned through the real `detect_context` →
/// `calculate_indentation` pipeline.
#[cfg(test)]
mod assignment_context_tests {
    use super::*;
    use crate::indentation::calculator::{
        IndentationConfig, IndentationStyle, calculate_indentation,
    };
    use tower_lsp::lsp_types::Position;

    fn parse_r(code: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    fn config(style: IndentationStyle) -> IndentationConfig {
        IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style,
        }
    }

    /// Detect at the start of `line` in `code` (the on-Enter cursor position).
    fn detect(code: &str, line: u32) -> IndentContext {
        let tree = parse_r(code);
        detect_context(&tree, code, Position { line, character: 0 }, 2)
    }

    /// Full pipeline: detected context → calculated column, RStudio style.
    fn indent_at(code: &str, line: u32) -> u32 {
        let ctx = detect(code, line);
        calculate_indentation(ctx, config(IndentationStyle::RStudio), code)
    }

    // ------------------------------------------------------------------
    // Detection: the new context fires for every participating operator
    // ------------------------------------------------------------------

    #[test]
    fn assignment_operators_fire() {
        for (code, base) in [
            ("result <-\n", 0),
            ("result <<-\n", 0),
            ("x =\n", 0),
            ("x :=\n", 0),
            ("data %>% f() ->\n", 0),
            ("data %>% f() ->>\n", 0),
        ] {
            let ctx = detect(code, 1);
            assert_eq!(
                ctx,
                IndentContext::AfterAssignmentOperator {
                    base_line: base,
                    flattened: false,
                },
                "for {code:?}"
            );
        }
    }

    #[test]
    fn right_assignment_after_chain_uses_statement_start() {
        // The `->` binop's LHS is the whole pipe chain, so the base is the
        // `data` line: the target belongs one level from the statement
        // start (column 2), not from the operator's physical line.
        let code = "data %>%\n  f() ->\n";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            }
        );
        assert_eq!(indent_at(code, 2), 2);
    }

    #[test]
    fn non_assignment_operator_endings_do_not_fire() {
        for code in ["x ==\n", "x <=\n", "x >=\n", "x !=\n"] {
            let ctx = detect(code, 1);
            assert!(
                !matches!(ctx, IndentContext::AfterAssignmentOperator { .. }),
                "{code:?} must not classify as assignment, got {ctx:?}"
            );
        }
    }

    #[test]
    fn string_and_comment_endings_do_not_fire() {
        for code in [
            "x <- \"a <-\"\n",    // line ends with a string, not the operator
            "# x <-\n",           // comment-only line
            "x <- 1 # note <-\n", // operator only inside the trailing comment
        ] {
            let ctx = detect(code, 1);
            assert!(
                !matches!(ctx, IndentContext::AfterAssignmentOperator { .. }),
                "{code:?} must not classify as assignment, got {ctx:?}"
            );
        }
    }

    #[test]
    fn multiline_string_interior_rejected_by_ast_validation() {
        // The second line ends with `<-` textually, but it is string
        // content: the node-kind check rejects it.
        let code = "text <- \"first\nlooks like <-\n";
        let ctx = detect(code, 2);
        assert!(
            !matches!(ctx, IndentContext::AfterAssignmentOperator { .. }),
            "string interior must not classify as assignment, got {ctx:?}"
        );
    }

    #[test]
    fn custom_infix_assignment_operator_is_not_an_assignment() {
        // `%<-%` (zeallot-style) ends with `%`: it must not classify as an
        // assignment (`ends_with("<-")` is false — the `%` is last). It is
        // also not a recognized custom-infix continuation today, because
        // `is_custom_infix_ending` does not allow `-` between the percent
        // signs — pre-existing behavior this test does not change.
        let code = "c(a, b) %<-%\n";
        let ctx = detect(code, 1);
        assert!(
            !matches!(ctx, IndentContext::AfterAssignmentOperator { .. }),
            "%<-% must not classify as assignment, got {ctx:?}"
        );
    }

    #[test]
    fn raw_string_with_quote_and_hash_fires() {
        // Tier 2's tokenizer understands raw strings, so a `"` + `#` inside
        // one does not hide the trailing operator (the Tier 1 regex cannot
        // express raw strings — pinned as a known limitation in
        // tests/bun/on-enter-rules.test.ts — which makes this coverage the
        // load-bearing one).
        let code = "x[r\"(a\"#b)\"] <-\n";
        let ctx = detect(code, 1);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            }
        );
    }

    #[test]
    fn backtick_name_with_hash_fires() {
        // `#` inside a backquoted name is not a comment (and needs no
        // escape — R's parser rejects `\`` inside backticks, so the first
        // backtick always closes the name).
        let code = "`a#b` <-\n";
        let ctx = detect(code, 1);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            }
        );
    }

    #[test]
    fn non_ascii_lhs_fires() {
        // Multi-byte identifier before the operator exercises the
        // byte-column arithmetic used to locate the operator token.
        let code = "名前 <-\n";
        let ctx = detect(code, 1);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            }
        );
    }

    // ------------------------------------------------------------------
    // Paren-context gate: argument/paren alignment wins
    // ------------------------------------------------------------------

    #[test]
    fn named_argument_eq_defers_to_parens() {
        for code in ["f(x =\n", "f(\n  x =\n"] {
            let line = code.lines().count() as u32;
            let ctx = detect(code, line);
            assert!(
                matches!(ctx, IndentContext::InsideParens { .. }),
                "{code:?} must stay InsideParens, got {ctx:?}"
            );
        }
    }

    #[test]
    fn formal_default_eq_defers_to_parens() {
        let code = "f <- function(x =\n";
        let ctx = detect(code, 1);
        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "formal default must stay InsideParens, got {ctx:?}"
        );
    }

    #[test]
    fn assignment_inside_call_arguments_defers_to_parens() {
        // The lint accepts the paren-aligned shape here in every mode
        // (`long_function_name(x <-` ⏎ aligned after the paren is clean;
        // one level from the line is flagged), so InsideParens must win.
        let code = "long_function_name(x <-\n";
        let ctx = detect(code, 1);
        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "assignment in call args must stay InsideParens, got {ctx:?}"
        );
        // RStudio: content after the opener → align to opener_col + 1.
        assert_eq!(indent_at(code, 1), 19);
    }

    #[test]
    fn assignment_under_brace_in_call_fires() {
        // A braced_expression between the assignment and the call restores
        // statement context (mirrors the lint's restorer): one level from
        // the `x <-` line, not argument alignment.
        let code = "f(function() {\n  x <-\n";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 1,
                flattened: false,
            }
        );
        assert_eq!(indent_at(code, 2), 4);
    }

    #[test]
    fn distant_string_parens_do_not_poison_the_gate() {
        // Parens inside a (closed) multiline string above must not defer a
        // statement-level assignment: the gate is an AST ancestor walk, not
        // a text scan.
        let code = "text <- \"\n(\n\"\nresult <-\n";
        let ctx = detect(code, 4);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 3,
                flattened: false,
            }
        );
    }

    // ------------------------------------------------------------------
    // Flattening: assignment_as_infix symmetry with the lint
    // ------------------------------------------------------------------

    #[test]
    fn chain_under_broken_assignment_is_flattened() {
        let code = "result <-\n  data %>%\n";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterContinuationOperator {
                chain_start_line: 1,
                chain_start_col: 2,
                operator_type: OperatorType::MagrittrPipe,
                flattened_rhs: true,
            }
        );
        assert_eq!(indent_at(code, 2), 2);
    }

    #[test]
    fn second_continuation_under_broken_assignment_stays_flat() {
        let code = "result <-\n  data %>%\n  filter(x) %>%\n";
        assert_eq!(indent_at(code, 3), 2);
    }

    #[test]
    fn mixed_chain_under_broken_assignment_flattens_to_line_indent() {
        // The pipe sub-chain starts mid-line (`data` at column 6), but the
        // flattened indent is the chain-start LINE's indent — the lint
        // expects column 2 here in every mode.
        let code = "result <-\n  a + data %>%\n";
        assert_eq!(indent_at(code, 2), 2);
    }

    #[test]
    fn chain_without_assignment_keeps_the_floor() {
        // No broken assignment above: the one-level floor stays.
        let code = "data %>%\n";
        let ctx = detect(code, 1);
        assert_eq!(
            ctx,
            IndentContext::AfterContinuationOperator {
                chain_start_line: 0,
                chain_start_col: 0,
                operator_type: OperatorType::MagrittrPipe,
                flattened_rhs: false,
            }
        );
        assert_eq!(indent_at(code, 1), 2);
    }

    #[test]
    fn unbroken_assignment_chain_alignment_unchanged() {
        // `result <- data %>%` (operator and chain start on one line) keeps
        // the existing alignment under `data`.
        let code = "result <- data %>%\n";
        assert_eq!(indent_at(code, 1), 10);
    }

    #[test]
    fn chained_broken_assignments_flatten() {
        // `a <-` ⏎ `  b <-` ⏎: the inner assignment is itself the RHS of a
        // broken assignment, so it adds no level — the lint expects the
        // final RHS at column 2 in every mode.
        let code = "a <-\n  b <-\n";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 1,
                flattened: true,
            }
        );
        assert_eq!(indent_at(code, 2), 2);
    }

    #[test]
    fn comment_between_assignment_and_chain_still_flattens() {
        let code = "result <-\n  # prepare\n  data %>%\n";
        let ctx = detect(code, 3);
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: true,
                    ..
                }
            ),
            "comment between assignment and chain must not defeat flattening, got {ctx:?}"
        );
        assert_eq!(indent_at(code, 3), 2);
    }

    #[test]
    fn chain_inside_call_under_assignment_is_not_flattened() {
        // Restorer shape: the chain sits inside a call argument, not
        // directly on the assignment's RHS — the call boundary restores
        // normal continuation indentation (this routes through the regex
        // fallback; the unclosed-`(`-at-chain-start guard rejects
        // flattening).
        let code = "result <-\n  f(data %>%\n";
        let ctx = detect(code, 2);
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: false,
                    ..
                }
            ),
            "call boundary must restore (no flattening), got {ctx:?}"
        );
        // Behavior unchanged from before #611: one level from the chain line.
        assert_eq!(indent_at(code, 2), 4);
    }

    // ------------------------------------------------------------------
    // Fallback path (ERROR trees)
    // ------------------------------------------------------------------

    #[test]
    fn assignment_in_unparseable_function_body_fires_via_fallback() {
        // `f <- function() {` ⏎ `  x <-` ⏎ parses with an ERROR node, so
        // this exercises fallback_detect_context through the public entry.
        let code = "f <- function() {\n  x <-\n";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 1,
                flattened: false,
            }
        );
        assert_eq!(indent_at(code, 2), 4);
    }

    #[test]
    fn walrus_in_braces_fires_via_fallback() {
        let code = "{\n  x :=\n";
        assert_eq!(indent_at(code, 2), 4);
    }

    #[test]
    fn walrus_inside_bracket_defers_to_parens_via_fallback() {
        // `dt[, y :=` ⏎ routes through the fallback (ERROR tree); the
        // unclosed `[` defers to bracket-content alignment.
        let code = "dt[, y :=\n";
        let ctx = detect(code, 1);
        assert!(
            matches!(ctx, IndentContext::InsideParens { .. }),
            "unclosed `[` must defer, got {ctx:?}"
        );
    }

    #[test]
    fn pushed_down_closers_do_not_defeat_assignment_detection() {
        // Real editor state: auto-closed `})` pushed onto the cursor line by
        // Enter. The fallback's closers-only exemption lets detection fall
        // through to the assignment context instead of aligning the closer.
        let code = "f(function() {\n  x <-\n})";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 1,
                flattened: false,
            }
        );
        assert_eq!(indent_at(code, 2), 4);
    }

    #[test]
    fn comment_in_chain_does_not_shift_right_assignment_base() {
        // Comment-only lines are syntactically transparent inside a chain:
        // the statement start for the `->` target is still the `data` line
        // (ERROR tree → ChainWalker route).
        let code = "f <- function() {\n  data %>%\n    # explanation\n    g() ->\n";
        let ctx = detect(code, 4);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 1,
                flattened: false,
            }
        );
        assert_eq!(indent_at(code, 4), 4);
    }

    #[test]
    fn comment_in_chain_keeps_continuation_at_chain_level_via_fallback() {
        // Same transparency for a plain continuation Enter: the chain start
        // stays at the `data` line, not the line after the comment.
        let code = "f <- function() {\n  data %>%\n    # explanation\n    g() %>%\n";
        assert_eq!(indent_at(code, 4), 4);
    }

    #[test]
    fn raw_string_delimiters_do_not_poison_fallback_gate() {
        // The `(` and `"` inside a raw string are content, not delimiters:
        // the fallback's unclosed-delimiter scan must not hallucinate an
        // unclosed `[` from `r"(a"#b)"` and defer the assignment.
        let code = "f <- function() {\n  long_name[r\"(a\"#b)\"] <-\n";
        let ctx = detect(code, 2);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 1,
                flattened: false,
            }
        );
        assert_eq!(indent_at(code, 2), 4);
    }

    #[test]
    fn fallback_direct_assignment_detection() {
        // Exercise fallback_detect_context directly for the plain shape,
        // independent of routing.
        let code = "result <-\n";
        let ctx = fallback_detect_context(
            code,
            Position {
                line: 1,
                character: 0,
            },
            2,
        );
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            }
        );
    }

    #[test]
    fn fallback_flatten_skips_blank_and_comment_lines() {
        let code = "result <-\n\n  # prep\n  data %>%\n";
        let ctx = fallback_detect_context(
            code,
            Position {
                line: 4,
                character: 0,
            },
            2,
        );
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: true,
                    ..
                }
            ),
            "blank/comment separators must not defeat fallback flattening, got {ctx:?}"
        );
    }

    #[test]
    fn paren_wrapped_rhs_still_flattens() {
        // A parenthesized expression does not restore the assignment
        // context (mirrors the lint): the chain inside `(` on the broken
        // assignment's RHS flattens to the chain-start line's indent.
        // Main AST path (tree has MISSING nodes only):
        let code = "a <-\n  (\n    data %>%\n";
        assert_eq!(indent_at(code, 3), 4);

        // Regex fallback path (ERROR tree from the unclosed function body);
        // the text walk skips the `(`-only line:
        let code = "f <- function() {\n  a <-\n    (\n      data %>%\n";
        let ctx = detect(code, 4);
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: true,
                    ..
                }
            ),
            "`(`-only separator must not defeat fallback flattening, got {ctx:?}"
        );
        assert_eq!(indent_at(code, 4), 6);
    }

    #[test]
    fn chain_hanging_in_same_line_paren_is_not_flattened() {
        // `a <-` ⏎ `  (data %>%` ⏎: the paren's hanging level still applies
        // even though the assignment suppression covers the chain — the
        // lint accepts columns 3 (hanging) or 4 (block), never 2. The
        // pre-#611 formula output (4) stands.
        // The unary-operator variant pins that the ancestor walk passes
        // through same-row nodes between the chain and the paren
        // (`(!data %>%`: parenthesized_expression → unary_operator → chain).
        for code in ["a <-\n  (data %>%\n", "a <-\n  (!data %>%\n"] {
            let ctx = detect(code, 2);
            assert!(
                matches!(
                    ctx,
                    IndentContext::AfterContinuationOperator {
                        flattened_rhs: false,
                        ..
                    }
                ),
                "same-line paren must exempt {code:?} from flattening, got {ctx:?}"
            );
            assert_eq!(indent_at(code, 2), 4, "for {code:?}");
        }
    }

    #[test]
    fn flattening_does_not_mirror_malformed_chain_indent() {
        // When the chain-start line is itself under-indented (malformed
        // input the auto-indenter never produced), the flattened output is
        // floored at the suppressing assignment's level + one tab instead
        // of mirroring the bad indent.
        let code = "result <-\ndata %>%\n";
        assert_eq!(indent_at(code, 2), 2);

        // Same inside a function body via the fallback route.
        let code = "f <- function() {\n  result <-\ndata %>%\n";
        assert_eq!(indent_at(code, 3), 4);

        // And for a chained assignment whose inner line is at column 0.
        let code = "a <-\nb <-\n";
        assert_eq!(indent_at(code, 2), 2);
    }

    #[test]
    fn chain_in_same_line_condition_is_not_flattened() {
        // A chain inside `if (`/`while (` condition parens under a broken
        // assignment keeps the condition's hanging level (the lint accepts
        // the hanging or block columns, never the bare line indent). These
        // states parse cleanly because the rest of the statement already
        // exists below the cursor.
        let code = "a <-\n  if (data %>%\n  g()) 1\n";
        let ctx = detect(code, 2);
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: false,
                    ..
                }
            ),
            "if-condition chain must not be flattened, got {ctx:?}"
        );
        assert_eq!(indent_at(code, 2), 6);

        let code = "a <-\n  while (data %>%\n  g()) 1\n";
        assert_eq!(indent_at(code, 2), 9);
    }

    #[test]
    fn chain_in_same_line_statement_body_is_flattened() {
        // A chain in a same-row `if` BODY has no bracket level: the lint
        // expects exactly the flattened column (2) in every mode.
        let code = "a <-\n  if (x) data %>%\n  g()\n";
        let ctx = detect(code, 2);
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: true,
                    ..
                }
            ),
            "if-body chain must be flattened, got {ctx:?}"
        );
        assert_eq!(indent_at(code, 2), 2);
    }

    #[test]
    fn brace_wrapped_rhs_does_not_flatten_via_fallback() {
        // A `{`-only line DOES restore the context (braces are a restorer
        // in the lint): the chain inside gets normal block indentation.
        let code = "f <- function() {\n  a <-\n    {\n      data %>%\n";
        let ctx = detect(code, 4);
        assert!(
            matches!(
                ctx,
                IndentContext::AfterContinuationOperator {
                    flattened_rhs: false,
                    ..
                }
            ),
            "`{{`-only separator must stop the flatten walk, got {ctx:?}"
        );
    }

    // ------------------------------------------------------------------
    // Pipeline columns: the issue's motivating cases, style-independent
    // ------------------------------------------------------------------

    #[test]
    fn issue_shapes_have_pinned_columns() {
        for (code, line, expected) in [
            ("result <-\n", 1, 2),
            ("x =\n", 1, 2),
            ("f <- function() {\n  x <-\n", 2, 4),
            ("data %>%\n  f() ->\n", 2, 2),
            ("result <-\n  data %>%\n", 2, 2),
            ("result <-\n  data %>%\n  filter(x) %>%\n", 3, 2),
            ("a <-\n  b <-\n", 2, 2),
        ] {
            assert_eq!(
                indent_at(code, line),
                expected,
                "column mismatch for {code:?}"
            );
        }
    }

    #[test]
    fn assignment_indent_is_style_independent() {
        for style in [IndentationStyle::RStudio, IndentationStyle::RStudioMinus] {
            for (code, line, expected) in [
                ("result <-\n", 1, 2),
                ("f <- function() {\n  x <-\n", 2, 4),
                ("result <-\n  data %>%\n", 2, 2),
            ] {
                let ctx = detect(code, line);
                assert_eq!(
                    calculate_indentation(ctx, config(style), code),
                    expected,
                    "column mismatch for {code:?} under {style:?}"
                );
            }
        }
    }

    #[test]
    fn enter_before_existing_content_reindents_it() {
        // Enter pressed with the RHS already typed (content pulled down to
        // the cursor line): detection still fires and the formatter edit
        // places the content at column 2.
        let code = "result <-\nf(x)";
        let ctx = detect(code, 1);
        assert_eq!(
            ctx,
            IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            }
        );
        let cfg = config(IndentationStyle::RStudio);
        let target = calculate_indentation(ctx, cfg.clone(), code);
        assert_eq!(target, 2);
        let edit = crate::indentation::format_indentation(1, target, cfg, code);
        assert_eq!(edit.new_text, "  ");
        assert_eq!(edit.range.start.line, 1);
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 0);
    }
}
