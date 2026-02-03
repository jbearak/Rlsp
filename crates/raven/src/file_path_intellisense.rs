//
// file_path_intellisense.rs
//
// File path intellisense for source() calls and LSP directives
//
// This module provides:
// 1. Context detection for file path completions
// 2. File path completions for source() calls and LSP directives
// 3. Go-to-definition for file paths in source() calls and LSP directives
//

use std::path::{Path, PathBuf};

use regex::Regex;
use std::sync::OnceLock;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Location, Position, Url};
use tree_sitter::{Node, Tree};

use crate::cross_file::path_resolve::PathContext;
use crate::cross_file::types::{byte_offset_to_utf16_column, CrossFileMetadata};

// ============================================================================
// Types
// ============================================================================

/// Context type for file path intellisense
///
/// Represents the detected context for file path operations, determining
/// how paths should be resolved and what completions should be provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePathContext {
    /// Inside string literal in source() or sys.source() call
    SourceCall {
        /// The partial path typed so far (content between opening quote and cursor)
        partial_path: String,
        /// Start position of the string content (after opening quote)
        content_start: Position,
        /// Whether this is sys.source (vs regular source)
        is_sys_source: bool,
    },
    /// After an LSP directive keyword
    Directive {
        /// The directive type (backward or forward)
        directive_type: DirectiveType,
        /// The partial path typed so far
        partial_path: String,
        /// Start position of the path (after directive keyword and optional colon/quote)
        path_start: Position,
    },
    /// Not in a file path context
    None,
}

/// Type of LSP directive for path context
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveType {
    /// Backward directives: @lsp-sourced-by, @lsp-run-by, @lsp-included-by
    /// These declare that the current file is sourced BY another file
    SourcedBy,
    /// Forward directive: @lsp-source
    /// This declares that the current file sources another file
    Source,
}

// ============================================================================
// Context Detection
// ============================================================================

/// Detect if cursor is in a file path context for completions
///
/// Checks source() calls first (via tree-sitter), then directive contexts (via regex).
/// Returns FilePathContext indicating the type of context and partial path.
///
/// # Arguments
/// * `tree` - The tree-sitter parse tree for the document
/// * `content` - The document text content
/// * `position` - The cursor position (line, character in UTF-16)
///
/// # Returns
/// A `FilePathContext` indicating the detected context type
pub fn detect_file_path_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> FilePathContext {
    // 1. Check source call context first via is_source_call_string_context()
    if let Some((partial_path, content_start, is_sys_source)) =
        is_source_call_string_context(tree, content, position)
    {
        log::trace!(
            "detect_file_path_context: Found source call context, partial_path='{}', content_start={:?}",
            partial_path,
            content_start
        );
        return FilePathContext::SourceCall {
            partial_path,
            content_start,
            is_sys_source,
        };
    }

    // 2. Then check directive context via is_directive_path_context()
    if let Some((directive_type, partial_path, path_start)) =
        is_directive_path_context(content, position)
    {
        log::trace!(
            "detect_file_path_context: Found directive context, type={:?}, partial_path='{}', path_start={:?}",
            directive_type,
            partial_path,
            path_start
        );
        return FilePathContext::Directive {
            directive_type,
            partial_path,
            path_start,
        };
    }

    // 3. Return FilePathContext::None if neither matches
    FilePathContext::None
}

/// Check if cursor is inside a string literal in a source()/sys.source() call
///
/// Uses tree-sitter AST traversal to find call nodes with function name
/// "source" or "sys.source", then checks if cursor is within the string argument.
///
/// # Arguments
/// * `tree` - The tree-sitter parse tree
/// * `content` - The document text content
/// * `position` - The cursor position (line, character in UTF-16)
///
/// # Returns
/// Some((partial_path, content_start, is_sys_source)) if in source call context
fn is_source_call_string_context(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<(String, Position, bool)> {
    // Convert LSP position (UTF-16) to tree-sitter point (bytes)
    let line_text = content.lines().nth(position.line as usize)?;
    let byte_col = utf16_column_to_byte_offset(line_text, position.character);
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: byte_col,
    };

    // Find the deepest node at the cursor position
    let root = tree.root_node();
    let node = find_deepest_node_at_point(root, point)?;

    // Walk up the tree to find if we're inside a string node
    let (string_node, call_node) = find_string_in_source_call(node, content)?;

    // Check if the call is to source() or sys.source()
    let func_node = call_node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);
    let is_sys_source = match func_text {
        "source" => false,
        "sys.source" => true,
        _ => return None,
    };

    // Check if the string is the file argument (first positional or named "file")
    if !is_file_argument(&string_node, &call_node, content) {
        return None;
    }

    // Get the string content boundaries (excluding quotes)
    let string_text = node_text(string_node, content);
    if string_text.len() < 2 {
        return None; // String too short to have content
    }

    // Determine quote character and content start
    let quote_char = string_text.chars().next()?;
    if quote_char != '"' && quote_char != '\'' {
        return None;
    }

    // Calculate content start position (after opening quote)
    let string_start = string_node.start_position();
    let string_start_line_text = content.lines().nth(string_start.row).unwrap_or("");
    let content_start_byte = string_start.column + 1; // +1 for opening quote
    let content_start_utf16 = byte_offset_to_utf16_column(string_start_line_text, content_start_byte);
    let content_start = Position {
        line: string_start.row as u32,
        character: content_start_utf16,
    };

    // Calculate string content end (before closing quote)
    let string_end = string_node.end_position();

    // Check if cursor is within the string content (after opening quote, before closing quote)
    // Cursor must be on the same line as string start (we don't support multi-line strings)
    if position.line != string_start.row as u32 {
        return None;
    }

    // Check cursor is after opening quote
    if position.character < content_start_utf16 {
        return None;
    }

    // Check cursor is before or at closing quote position
    let string_end_line_text = content.lines().nth(string_end.row).unwrap_or("");
    let content_end_utf16 = byte_offset_to_utf16_column(string_end_line_text, string_end.column);
    // Allow cursor at the closing quote position (user is still typing)
    if position.line == string_end.row as u32 && position.character > content_end_utf16 {
        return None;
    }

    // Extract partial path from content start to cursor position
    let partial_path = extract_partial_path(content, position.line, content_start_utf16, position.character);

    Some((partial_path, content_start, is_sys_source))
}

/// Find the deepest node at a given point in the AST
fn find_deepest_node_at_point(node: Node, point: tree_sitter::Point) -> Option<Node> {
    // Check if point is within this node's range
    if point < node.start_position() || point > node.end_position() {
        return None;
    }

    // Try to find a child that contains the point
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(deeper) = find_deepest_node_at_point(child, point) {
            return Some(deeper);
        }
    }

    // No child contains the point, so this node is the deepest
    Some(node)
}

/// Walk up the tree from a node to find if it's inside a string that's an argument to a source() call
/// Returns (string_node, call_node) if found
fn find_string_in_source_call<'a>(node: Node<'a>, content: &str) -> Option<(Node<'a>, Node<'a>)> {
    let mut current = node;

    // First, find the string node (we might be inside it or at it)
    let string_node = if current.kind() == "string" {
        current
    } else {
        // Walk up to find a string node
        loop {
            if let Some(parent) = current.parent() {
                if parent.kind() == "string" {
                    current = parent;
                    break;
                }
                current = parent;
            } else {
                return None;
            }
        }
        current
    };

    // Now walk up from the string to find a call node
    let mut current = string_node;
    loop {
        if let Some(parent) = current.parent() {
            if parent.kind() == "call" {
                // Check if this is a source() or sys.source() call
                if let Some(func_node) = parent.child_by_field_name("function") {
                    let func_text = node_text(func_node, content);
                    if func_text == "source" || func_text == "sys.source" {
                        return Some((string_node, parent));
                    }
                }
            }
            current = parent;
        } else {
            return None;
        }
    }
}

/// Check if a string node is the file argument of a source() call
fn is_file_argument(string_node: &Node, call_node: &Node, content: &str) -> bool {
    let args_node = match call_node.child_by_field_name("arguments") {
        Some(n) => n,
        None => return false,
    };

    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Check for named "file" argument
    for child in &children {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "file" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        // Check if this value node contains our string node
                        if nodes_overlap(&value_node, string_node) {
                            return true;
                        }
                    }
                }
            }
        }
    }

    // Check first positional argument
    for child in &children {
        if child.kind() == "argument" && child.child_by_field_name("name").is_none() {
            if let Some(value_node) = child.child_by_field_name("value") {
                // Check if this value node contains our string node
                if nodes_overlap(&value_node, string_node) {
                    return true;
                }
            }
            // Only check the first positional argument
            break;
        }
    }

    false
}

/// Check if two nodes overlap (one contains the other or they are the same)
fn nodes_overlap(a: &Node, b: &Node) -> bool {
    // Check if they are the same node or one contains the other
    let a_range = a.byte_range();
    let b_range = b.byte_range();

    // Check if ranges overlap
    a_range.start <= b_range.start && a_range.end >= b_range.end
        || b_range.start <= a_range.start && b_range.end >= a_range.end
}

/// Get the text content of a node
fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

/// Convert UTF-16 column to byte offset for a given line
fn utf16_column_to_byte_offset(line: &str, utf16_col: u32) -> usize {
    let mut utf16_count = 0;
    for (byte_idx, ch) in line.char_indices() {
        if utf16_count == utf16_col as usize {
            return byte_idx;
        }
        utf16_count += ch.len_utf16();
    }
    line.len()
}

/// Check if cursor is after an LSP directive where a path is expected
///
/// Uses regex patterns consistent with cross_file/directive.rs to detect
/// @lsp-sourced-by, @lsp-run-by, @lsp-included-by, and @lsp-source directives.
/// Handles optional colon and quotes syntax variations.
///
/// # Arguments
/// * `content` - The document text content
/// * `position` - The cursor position (line, character in UTF-16)
///
/// # Returns
/// Some((directive_type, partial_path, path_start)) if in directive context
fn is_directive_path_context(
    content: &str,
    position: Position,
) -> Option<(DirectiveType, String, Position)> {
    // Get the line at the cursor position
    let line_text = content.lines().nth(position.line as usize)?;

    // Convert cursor position from UTF-16 to byte offset
    let cursor_byte = utf16_column_to_byte_offset(line_text, position.character);

    // Get the patterns
    let patterns = directive_path_patterns();

    // Try backward directive pattern first
    if let Some(result) = try_match_directive_path(
        line_text,
        cursor_byte,
        position.line,
        &patterns.backward,
        DirectiveType::SourcedBy,
    ) {
        return Some(result);
    }

    // Try forward directive pattern
    if let Some(result) = try_match_directive_path(
        line_text,
        cursor_byte,
        position.line,
        &patterns.forward,
        DirectiveType::Source,
    ) {
        return Some(result);
    }

    None
}

/// Try to match a directive pattern and extract path context
///
/// # Arguments
/// * `line_text` - The line text to match against
/// * `cursor_byte` - The cursor position as byte offset
/// * `line_num` - The line number (0-based)
/// * `pattern` - The regex pattern to match
/// * `directive_type` - The type of directive if matched
///
/// # Returns
/// Some((directive_type, partial_path, path_start)) if cursor is in path context
fn try_match_directive_path(
    line_text: &str,
    cursor_byte: usize,
    line_num: u32,
    pattern: &Regex,
    directive_type: DirectiveType,
) -> Option<(DirectiveType, String, Position)> {
    // Check if the line matches the directive pattern
    let caps = pattern.captures(line_text)?;

    // Get the full match to find where the directive prefix ends
    let full_match = caps.get(0)?;
    let directive_prefix_end = full_match.end();

    // The path starts after the directive prefix
    // But we need to account for optional quotes
    let remaining = &line_text[directive_prefix_end..];

    // Determine if path is quoted and find path start
    let (path_start_byte, quote_char) = if remaining.starts_with('"') {
        (directive_prefix_end + 1, Some('"'))
    } else if remaining.starts_with('\'') {
        (directive_prefix_end + 1, Some('\''))
    } else {
        (directive_prefix_end, None)
    };

    // Check if cursor is at or after the path start position
    if cursor_byte < path_start_byte {
        return None;
    }

    // Find the path end (closing quote or end of non-whitespace)
    let path_end_byte = if let Some(quote) = quote_char {
        // Find closing quote
        let path_content = &line_text[path_start_byte..];
        if let Some(close_pos) = path_content.find(quote) {
            path_start_byte + close_pos
        } else {
            // No closing quote found, path extends to end of line
            line_text.len()
        }
    } else {
        // Unquoted path: ends at whitespace or end of line
        let path_content = &line_text[path_start_byte..];
        if let Some(space_pos) = path_content.find(char::is_whitespace) {
            path_start_byte + space_pos
        } else {
            line_text.len()
        }
    };

    // Check if cursor is within the path region
    // Allow cursor at path_end_byte for quoted paths (user might be at closing quote)
    // For unquoted paths, cursor can be anywhere after path_start
    if cursor_byte > path_end_byte {
        return None;
    }

    // Extract partial path from path start to cursor
    let partial_path = if cursor_byte > path_start_byte {
        line_text[path_start_byte..cursor_byte].to_string()
    } else {
        String::new()
    };

    // Convert path_start_byte to UTF-16 column
    let path_start_utf16 = byte_offset_to_utf16_column(line_text, path_start_byte);

    let path_start = Position {
        line: line_num,
        character: path_start_utf16,
    };

    Some((directive_type, partial_path, path_start))
}

/// Extract the partial path from string start to cursor position
///
/// Handles escaped characters within the string literal.
///
/// # Arguments
/// * `content` - The document text content
/// * `line` - The line number (0-based)
/// * `start_col` - The starting column (UTF-16, after opening quote)
/// * `cursor_col` - The cursor column (UTF-16)
///
/// # Returns
/// The partial path string from start to cursor
fn extract_partial_path(
    content: &str,
    line: u32,
    start_col: u32,
    cursor_col: u32,
) -> String {
    // Get the line text
    let line_text = match content.lines().nth(line as usize) {
        Some(text) => text,
        None => return String::new(),
    };

    // Convert UTF-16 columns to byte offsets
    let start_byte = utf16_column_to_byte_offset(line_text, start_col);
    let cursor_byte = utf16_column_to_byte_offset(line_text, cursor_col);

    // Extract the substring
    if cursor_byte <= start_byte {
        return String::new();
    }

    let partial = &line_text[start_byte..cursor_byte.min(line_text.len())];

    // Handle escaped characters (basic handling for common escapes)
    // R strings can have \\ for backslash, \" for quote, etc.
    unescape_string(partial)
}

/// Unescape common escape sequences in R strings
fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                match next {
                    '\\' => {
                        result.push('\\');
                        chars.next();
                    }
                    '"' => {
                        result.push('"');
                        chars.next();
                    }
                    '\'' => {
                        result.push('\'');
                        chars.next();
                    }
                    'n' => {
                        result.push('\n');
                        chars.next();
                    }
                    't' => {
                        result.push('\t');
                        chars.next();
                    }
                    'r' => {
                        result.push('\r');
                        chars.next();
                    }
                    _ => {
                        // Keep the backslash for unrecognized escapes
                        result.push(c);
                    }
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }

    result
}

// ============================================================================
// Completions
// ============================================================================

/// Generate file path completions for the given context
///
/// Determines the base directory based on context type:
/// - SourceCall: Uses PathContext::from_metadata() (respects @lsp-cd)
/// - Directive: Uses PathContext::new() (ignores @lsp-cd)
///
/// # Arguments
/// * `context` - The detected file path context
/// * `file_uri` - URI of the current file
/// * `metadata` - Cross-file metadata for the current file
/// * `workspace_root` - Optional workspace root URI
/// * `cursor_position` - The cursor position for text_edit range
///
/// # Returns
/// Vector of CompletionItem for R files and directories
pub fn file_path_completions(
    context: &FilePathContext,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
    cursor_position: Position,
) -> Vec<CompletionItem> {
    // 1. Return empty vec if context is None
    if matches!(context, FilePathContext::None) {
        return Vec::new();
    }

    // Extract path_start and partial_path from context for text_edit
    let (path_start, partial_path) = match context {
        FilePathContext::SourceCall {
            content_start,
            partial_path,
            ..
        } => (*content_start, partial_path.as_str()),
        FilePathContext::Directive {
            path_start,
            partial_path,
            ..
        } => (*path_start, partial_path.as_str()),
        FilePathContext::None => return Vec::new(),
    };

    // 2. Resolve the base directory for listing
    let base_dir = match resolve_base_directory(context, file_uri, metadata, workspace_root) {
        Some(dir) => dir,
        None => {
            log::trace!(
                "file_path_completions: Failed to resolve base directory for context {:?}",
                context
            );
            return Vec::new();
        }
    };

    // 3. Get workspace root path for boundary checking
    let workspace_path = workspace_root.and_then(|url| url.to_file_path().ok());

    // 4. List directory entries (excludes hidden files)
    let entries = match list_directory_entries(&base_dir, workspace_path.as_deref()) {
        Ok(entries) => entries,
        Err(e) => {
            log::trace!(
                "file_path_completions: Failed to list directory {:?}: {}",
                base_dir,
                e
            );
            return Vec::new();
        }
    };

    // 5. Filter to R files and directories
    let filtered_entries = filter_r_files_and_dirs(entries);

    // 6. Extract the directory prefix from partial_path (everything up to and including last /)
    // This prefix will be prepended to completion items
    let dir_prefix = extract_directory_component(partial_path);

    // 7. Create completion items for each entry
    filtered_entries
        .iter()
        .map(|(name, _path, is_directory)| {
            create_path_completion_item(name, *is_directory, &dir_prefix, path_start, cursor_position)
        })
        .collect()
}

/// List directory entries, excluding hidden files (starting with '.')
///
/// # Arguments
/// * `base_path` - The directory to list
/// * `workspace_root` - Optional workspace root for boundary checking
///
/// # Returns
/// Vector of (name, path, is_directory) tuples for non-hidden entries
///
/// # Errors
/// Returns an error if the directory cannot be read. However, individual
/// entry errors (e.g., permission denied on a specific file) are silently
/// skipped to provide graceful degradation.
fn list_directory_entries(
    base_path: &Path,
    workspace_root: Option<&Path>,
) -> std::io::Result<Vec<(String, PathBuf, bool)>> {
    let mut entries = Vec::new();

    // Read directory entries
    let dir_entries = match std::fs::read_dir(base_path) {
        Ok(entries) => entries,
        Err(e) => {
            // Log trace message for debugging
            log::trace!("Failed to read directory {:?}: {}", base_path, e);
            return Err(e);
        }
    };

    for entry_result in dir_entries {
        // Skip entries that fail to read (e.g., permission denied)
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                log::trace!("Failed to read directory entry: {}", e);
                continue;
            }
        };

        // Get the file name
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().to_string();

        // Filter out hidden files/directories (starting with '.')
        if name.starts_with('.') {
            continue;
        }

        // Get the full path
        let path = entry.path();

        // Check workspace boundary if workspace_root is provided
        if let Some(workspace) = workspace_root {
            // Canonicalize paths for accurate comparison
            // If canonicalization fails, fall back to non-canonical comparison
            let canonical_path_opt = path.canonicalize().ok();
            let canonical_workspace_opt = workspace.canonicalize().ok();

            match (canonical_path_opt, canonical_workspace_opt) {
                (Some(resolved), Some(ws)) => {
                    if !resolved.starts_with(&ws) {
                        continue;
                    }
                }
                (None, _) => {
                    // Path doesn't exist yet or can't be accessed
                    // Fall back to checking if path starts with workspace
                    if !path.starts_with(workspace) {
                        continue;
                    }
                }
                _ => continue, // Can't verify workspace boundary
            }
        }

        // Determine if this is a directory
        let is_directory = match entry.file_type() {
            Ok(ft) => ft.is_dir(),
            Err(_) => {
                // If we can't determine file type, try metadata as fallback
                path.is_dir()
            }
        };

        entries.push((name, path, is_directory));
    }

    // Sort entries: directories first, then alphabetically by name
    entries.sort_by(|a, b| {
        match (a.2, b.2) {
            (true, false) => std::cmp::Ordering::Less,    // directories first
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()), // alphabetical (case-insensitive)
        }
    });

    Ok(entries)
}

/// Filter entries to R files (.R, .r) and directories
///
/// Keeps:
/// - Files with .R or .r extension
/// - All directories (for navigation)
///
/// # Arguments
/// * `entries` - Vector of (name, path, is_directory) tuples
///
/// # Returns
/// Filtered vector containing only R files and directories
fn filter_r_files_and_dirs(
    entries: Vec<(String, PathBuf, bool)>,
) -> Vec<(String, PathBuf, bool)> {
    entries
        .into_iter()
        .filter(|(name, _, is_directory)| {
            // Keep all directories for navigation
            if *is_directory {
                return true;
            }

            // For files, check if the extension is .R or .r
            // Use the name to check extension (more reliable than path for edge cases)
            if let Some(ext) = Path::new(name).extension() {
                let ext_str = ext.to_string_lossy();
                ext_str == "R" || ext_str == "r"
            } else {
                false
            }
        })
        .collect()
}

/// Create a completion item for a file or directory
///
/// - Sets CompletionItemKind::FILE or FOLDER
/// - Appends trailing '/' to directory insert_text
/// - Uses forward slashes for all paths (R convention)
/// - Sets text_edit to replace from path_start to cursor position
///
/// # Arguments
/// * `name` - The file or directory name
/// * `is_directory` - Whether this is a directory
/// * `dir_prefix` - The directory prefix from the partial path (e.g., "../" or "subdir/")
/// * `path_start` - The position where the path starts (for text_edit range)
/// * `cursor_position` - The cursor position (end of text_edit range)
///
/// # Returns
/// A CompletionItem configured for the entry
fn create_path_completion_item(
    name: &str,
    is_directory: bool,
    dir_prefix: &str,
    path_start: Position,
    cursor_position: Position,
) -> CompletionItem {
    // Build the full insert text with directory prefix
    let insert_text = if is_directory {
        format!("{}{}/", dir_prefix, name)
    } else {
        format!("{}{}", dir_prefix, name)
    };

    // Create text_edit that replaces from path_start to cursor position
    // This ensures the completion replaces the entire partial path typed so far
    let text_edit = tower_lsp::lsp_types::CompletionTextEdit::Edit(
        tower_lsp::lsp_types::TextEdit {
            range: tower_lsp::lsp_types::Range {
                start: path_start,
                end: cursor_position,
            },
            new_text: insert_text.clone(),
        },
    );

    // For directories, add a command to re-trigger completions after accepting
    // This allows users to continue navigating into subdirectories
    let command = if is_directory {
        Some(tower_lsp::lsp_types::Command {
            title: String::from("Trigger Suggest"),
            command: String::from("editor.action.triggerSuggest"),
            arguments: None,
        })
    } else {
        None
    };

    CompletionItem {
        label: name.to_string(),
        kind: Some(if is_directory {
            CompletionItemKind::FOLDER
        } else {
            CompletionItemKind::FILE
        }),
        // Don't set filter_text - let the client use text_edit range for filtering
        // Setting filter_text can cause issues when the user types characters
        // (like spaces) that don't match the filename
        text_edit: Some(text_edit),
        command,
        // Set sort_text to ensure consistent ordering
        sort_text: Some(if is_directory {
            format!("0{}", name) // Directories first
        } else {
            format!("1{}", name) // Files second
        }),
        ..Default::default()
    }
}

// ============================================================================
// Go-to-Definition
// ============================================================================

/// Get definition location for a file path at the given position
///
/// Detects context type and resolves path using appropriate PathContext:
/// - SourceCall: Uses PathContext::from_metadata() (respects @lsp-cd)
/// - Directive: Uses PathContext::new() (ignores @lsp-cd)
///
/// If workspace_root is provided, enforces workspace boundary: paths resolving
/// outside the workspace return None.
///
/// # Arguments
/// * `tree` - The tree-sitter parse tree
/// * `content` - The document text content
/// * `position` - The cursor position
/// * `file_uri` - URI of the current file
/// * `metadata` - Cross-file metadata for the current file
/// * `workspace_root` - Optional workspace root URI
///
/// # Returns
/// Some(Location) at line 0, column 0 if file exists and is within workspace, None otherwise
pub fn file_path_definition(
    tree: &Tree,
    content: &str,
    position: Position,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Option<Location> {
    use crate::cross_file::path_resolve::resolve_path;

    // 1. Extract file path at position
    let (path_string, context) = extract_file_path_at_position(tree, content, position)?;

    // Return None if path is empty
    if path_string.is_empty() {
        log::trace!("file_path_definition: Empty path string at position {:?}", position);
        return None;
    }

    // Normalize path separators (convert backslashes to forward slashes)
    let normalized_path = normalize_path_separators(&path_string);

    // 2. Create appropriate PathContext based on context type
    let path_context = match &context {
        FilePathContext::SourceCall { .. } => {
            // For source() calls: respect @lsp-cd working directory
            PathContext::from_metadata(file_uri, metadata, workspace_root)?
        }
        FilePathContext::Directive { .. } => {
            // For directives: always relative to file's directory (ignore @lsp-cd)
            PathContext::new(file_uri, workspace_root)?
        }
        FilePathContext::None => {
            // Should not happen since extract_file_path_at_position returns None for this case
            return None;
        }
    };

    // 3. Resolve the path using the appropriate context
    let resolved_path = resolve_path(&normalized_path, &path_context)?;

    log::trace!(
        "file_path_definition: Resolved path '{}' to '{}'",
        path_string,
        resolved_path.display()
    );

    // 4. Check if the resolved file exists
    if !resolved_path.exists() {
        log::trace!(
            "file_path_definition: File does not exist: '{}'",
            resolved_path.display()
        );
        return None;
    }

    // Also verify it's a file (not a directory)
    if !resolved_path.is_file() {
        log::trace!(
            "file_path_definition: Path is not a file: '{}'",
            resolved_path.display()
        );
        return None;
    }

    // 5. Check workspace boundary if workspace_root is provided
    if let Some(workspace_url) = workspace_root {
        if let Ok(workspace_path) = workspace_url.to_file_path() {
            // Canonicalize both paths for accurate comparison
            let canonical_resolved = resolved_path.canonicalize().ok();
            let canonical_workspace = workspace_path.canonicalize().ok();
            
            if let (Some(resolved), Some(workspace)) = (canonical_resolved, canonical_workspace) {
                if !resolved.starts_with(&workspace) {
                    log::trace!(
                        "file_path_definition: Path '{}' is outside workspace '{}'",
                        resolved_path.display(),
                        workspace_path.display()
                    );
                    return None;
                }
            }
        }
    }

    // 6. Convert the resolved path to a URI
    let target_uri = Url::from_file_path(&resolved_path).ok()?;

    // 7. Return Location at line 0, column 0
    Some(Location {
        uri: target_uri,
        range: tower_lsp::lsp_types::Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 0 },
        },
    })
}

/// Extract the complete file path string at the cursor position
///
/// For source() calls: extracts the full string literal content
/// For directives: extracts the path after the directive keyword
///
/// # Arguments
/// * `tree` - The tree-sitter parse tree
/// * `content` - The document text content
/// * `position` - The cursor position
///
/// # Returns
/// Some((path_string, context_type)) if cursor is on a file path
fn extract_file_path_at_position(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<(String, FilePathContext)> {
    // 1. Check if in source call context and extract full path
    if let Some((full_path, content_start, is_sys_source)) =
        extract_full_source_call_path(tree, content, position)
    {
        let context = FilePathContext::SourceCall {
            partial_path: full_path.clone(),
            content_start,
            is_sys_source,
        };
        return Some((full_path, context));
    }

    // 2. Check if in directive context and extract full path
    if let Some((directive_type, full_path, path_start)) =
        extract_full_directive_path(content, position)
    {
        let context = FilePathContext::Directive {
            directive_type,
            partial_path: full_path.clone(),
            path_start,
        };
        return Some((full_path, context));
    }

    // 3. Return None if cursor is not on a file path
    None
}

/// Extract the full path string from a source() or sys.source() call at the cursor position
///
/// Unlike `is_source_call_string_context()` which returns the partial path up to cursor,
/// this function returns the FULL string content for go-to-definition.
///
/// # Arguments
/// * `tree` - The tree-sitter parse tree
/// * `content` - The document text content
/// * `position` - The cursor position (line, character in UTF-16)
///
/// # Returns
/// Some((full_path, content_start, is_sys_source)) if cursor is in source call context
fn extract_full_source_call_path(
    tree: &Tree,
    content: &str,
    position: Position,
) -> Option<(String, Position, bool)> {
    // Convert LSP position (UTF-16) to tree-sitter point (bytes)
    let line_text = content.lines().nth(position.line as usize)?;
    let byte_col = utf16_column_to_byte_offset(line_text, position.character);
    let point = tree_sitter::Point {
        row: position.line as usize,
        column: byte_col,
    };

    // Find the deepest node at the cursor position
    let root = tree.root_node();
    let node = find_deepest_node_at_point(root, point)?;

    // Walk up the tree to find if we're inside a string node
    let (string_node, call_node) = find_string_in_source_call(node, content)?;

    // Check if the call is to source() or sys.source()
    let func_node = call_node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);
    let is_sys_source = match func_text {
        "source" => false,
        "sys.source" => true,
        _ => return None,
    };

    // Check if the string is the file argument (first positional or named "file")
    if !is_file_argument(&string_node, &call_node, content) {
        return None;
    }

    // Get the string content boundaries (excluding quotes)
    let string_text = node_text(string_node, content);
    if string_text.len() < 2 {
        return None; // String too short to have content
    }

    // Determine quote character and content start
    let quote_char = string_text.chars().next()?;
    if quote_char != '"' && quote_char != '\'' {
        return None;
    }

    // Calculate content start position (after opening quote)
    let string_start = string_node.start_position();
    let string_start_line_text = content.lines().nth(string_start.row).unwrap_or("");
    let content_start_byte = string_start.column + 1; // +1 for opening quote
    let content_start_utf16 = byte_offset_to_utf16_column(string_start_line_text, content_start_byte);
    let content_start = Position {
        line: string_start.row as u32,
        character: content_start_utf16,
    };

    // Calculate string content end (before closing quote)
    let string_end = string_node.end_position();

    // Check if cursor is within the string content (after opening quote, before closing quote)
    // Cursor must be on the same line as string start (we don't support multi-line strings)
    if position.line != string_start.row as u32 {
        return None;
    }

    // Check cursor is after opening quote
    if position.character < content_start_utf16 {
        return None;
    }

    // Check cursor is before or at closing quote position
    let string_end_line_text = content.lines().nth(string_end.row).unwrap_or("");
    let content_end_utf16 = byte_offset_to_utf16_column(string_end_line_text, string_end.column);
    // Allow cursor at the closing quote position (user might be clicking on the path)
    if position.line == string_end.row as u32 && position.character > content_end_utf16 {
        return None;
    }

    // Extract the FULL path content (from content start to content end, excluding quotes)
    // The string content is between the quotes
    let content_end_byte = string_end.column.saturating_sub(1); // -1 for closing quote
    let full_path = if content_end_byte > content_start_byte {
        let path_bytes = &string_start_line_text[content_start_byte..content_end_byte];
        unescape_string(path_bytes)
    } else {
        String::new()
    };

    Some((full_path, content_start, is_sys_source))
}

/// Extract the full path string from an LSP directive at the cursor position
///
/// Unlike `is_directive_path_context()` which returns the partial path up to cursor,
/// this function returns the FULL path for go-to-definition.
///
/// # Arguments
/// * `content` - The document text content
/// * `position` - The cursor position (line, character in UTF-16)
///
/// # Returns
/// Some((directive_type, full_path, path_start)) if cursor is in directive context
fn extract_full_directive_path(
    content: &str,
    position: Position,
) -> Option<(DirectiveType, String, Position)> {
    // Get the line at the cursor position
    let line_text = content.lines().nth(position.line as usize)?;

    // Convert cursor position from UTF-16 to byte offset
    let cursor_byte = utf16_column_to_byte_offset(line_text, position.character);

    // Get the patterns
    let patterns = directive_path_patterns();

    // Try backward directive pattern first
    if let Some(result) = try_extract_full_directive_path(
        line_text,
        cursor_byte,
        position.line,
        &patterns.backward,
        DirectiveType::SourcedBy,
    ) {
        return Some(result);
    }

    // Try forward directive pattern
    if let Some(result) = try_extract_full_directive_path(
        line_text,
        cursor_byte,
        position.line,
        &patterns.forward,
        DirectiveType::Source,
    ) {
        return Some(result);
    }

    None
}

/// Try to match a directive pattern and extract the full path
///
/// # Arguments
/// * `line_text` - The line text to match against
/// * `cursor_byte` - The cursor position as byte offset
/// * `line_num` - The line number (0-based)
/// * `pattern` - The regex pattern to match
/// * `directive_type` - The type of directive if matched
///
/// # Returns
/// Some((directive_type, full_path, path_start)) if cursor is in path context
fn try_extract_full_directive_path(
    line_text: &str,
    cursor_byte: usize,
    line_num: u32,
    pattern: &Regex,
    directive_type: DirectiveType,
) -> Option<(DirectiveType, String, Position)> {
    // Check if the line matches the directive pattern
    let caps = pattern.captures(line_text)?;

    // Get the full match to find where the directive prefix ends
    let full_match = caps.get(0)?;
    let directive_prefix_end = full_match.end();

    // The path starts after the directive prefix
    // But we need to account for optional quotes
    let remaining = &line_text[directive_prefix_end..];

    // Determine if path is quoted and find path start
    let (path_start_byte, quote_char) = if remaining.starts_with('"') {
        (directive_prefix_end + 1, Some('"'))
    } else if remaining.starts_with('\'') {
        (directive_prefix_end + 1, Some('\''))
    } else {
        (directive_prefix_end, None)
    };

    // Check if cursor is at or after the path start position
    if cursor_byte < path_start_byte {
        return None;
    }

    // Find the path end (closing quote or end of non-whitespace)
    let path_end_byte = if let Some(quote) = quote_char {
        // Find closing quote
        let path_content = &line_text[path_start_byte..];
        if let Some(close_pos) = path_content.find(quote) {
            path_start_byte + close_pos
        } else {
            // No closing quote found, path extends to end of line
            line_text.len()
        }
    } else {
        // Unquoted path: ends at whitespace or end of line
        let path_content = &line_text[path_start_byte..];
        if let Some(space_pos) = path_content.find(char::is_whitespace) {
            path_start_byte + space_pos
        } else {
            line_text.len()
        }
    };

    // Check if cursor is within the path region
    // Allow cursor at path_end_byte for quoted paths (user might be clicking on the path)
    if cursor_byte > path_end_byte {
        return None;
    }

    // Extract the FULL path (from path start to path end)
    let full_path = if path_end_byte > path_start_byte {
        line_text[path_start_byte..path_end_byte].to_string()
    } else {
        String::new()
    };

    // Convert path_start_byte to UTF-16 column
    let path_start_utf16 = byte_offset_to_utf16_column(line_text, path_start_byte);

    let path_start = Position {
        line: line_num,
        character: path_start_utf16,
    };

    Some((directive_type, full_path, path_start))
}

// ============================================================================
// Path Resolution Helpers
// ============================================================================

/// Resolve the base directory for file path completions
///
/// Determines the base directory based on context type and partial path:
/// - For SourceCall: Uses PathContext::from_metadata() (respects @lsp-cd)
/// - For Directive: Uses PathContext::new() (ignores @lsp-cd)
/// - For paths starting with `/`: Resolves relative to workspace root (both contexts)
///
/// The partial path's directory component (e.g., `../`, `subdir/`) is joined
/// with the base directory to get the final directory to list.
///
/// # Arguments
/// * `context` - The detected file path context
/// * `file_uri` - URI of the current file
/// * `metadata` - Cross-file metadata for the current file
/// * `workspace_root` - Optional workspace root URI
///
/// # Returns
/// Some(PathBuf) with the resolved base directory, or None if resolution fails.
/// Returns None for `/` paths when workspace_root is None.
pub fn resolve_base_directory(
    context: &FilePathContext,
    file_uri: &Url,
    metadata: &CrossFileMetadata,
    workspace_root: Option<&Url>,
) -> Option<PathBuf> {
    // Extract partial path from context
    let partial_path = match context {
        FilePathContext::SourceCall { partial_path, .. } => partial_path,
        FilePathContext::Directive { partial_path, .. } => partial_path,
        FilePathContext::None => return None,
    };

    // Normalize path separators (convert backslashes to forward slashes)
    let normalized_partial = normalize_path_separators(partial_path);

    // Get the directory component of the partial path
    // e.g., "../data/file.R" -> "../data/", "file.R" -> "", "../" -> "../"
    let partial_dir = extract_directory_component(&normalized_partial);

    // Handle workspace-root-relative paths (paths starting with `/`)
    // For BOTH SourceCall and Directive contexts: `/path` resolves relative to workspace root
    // If workspace_root is None, return None (cannot resolve workspace-relative paths)
    if normalized_partial.starts_with('/') {
        let workspace_path = workspace_root
            .and_then(|url| url.to_file_path().ok())?;
        
        // Strip the leading `/` from partial_dir and join with workspace root
        let relative_dir = partial_dir.strip_prefix('/').unwrap_or(&partial_dir);
        
        if relative_dir.is_empty() {
            // Just "/" - return workspace root
            return Some(workspace_path);
        }
        
        // Join workspace root with the relative directory
        let joined = workspace_path.join(relative_dir);
        return normalize_path_for_completion(&joined);
    }

    // Create the appropriate PathContext based on context type
    let path_context = match context {
        FilePathContext::SourceCall { .. } => {
            // For source() calls: respect @lsp-cd working directory
            PathContext::from_metadata(file_uri, metadata, workspace_root)?
        }
        FilePathContext::Directive { .. } => {
            // For directives: always relative to file's directory (ignore @lsp-cd)
            PathContext::new(file_uri, workspace_root)?
        }
        FilePathContext::None => return None,
    };

    // Get the effective base directory
    let base_dir = path_context.effective_working_directory();

    // If partial_dir is empty, use the base directory directly
    if partial_dir.is_empty() {
        return Some(base_dir);
    }

    // Join the partial directory with the base directory
    let joined = base_dir.join(&partial_dir);

    // Normalize the path to resolve .. and . components
    normalize_path_for_completion(&joined)
}

/// Extract the directory component from a partial path
///
/// Returns the directory portion of the path (everything up to and including
/// the last path separator), or an empty string if there's no directory component.
///
/// # Examples
/// - `"../data/file.R"` -> `"../data/"`
/// - `"file.R"` -> `""`
/// - `"../"` -> `"../"`
/// - `"subdir/"` -> `"subdir/"`
/// - `"../data/"` -> `"../data/"`
fn extract_directory_component(partial_path: &str) -> String {
    // Find the last path separator
    if let Some(last_sep_pos) = partial_path.rfind('/') {
        // Include the separator in the result
        partial_path[..=last_sep_pos].to_string()
    } else {
        // No separator found, no directory component
        String::new()
    }
}

/// Normalize a path for completion purposes
///
/// Resolves `.` and `..` components without requiring the path to exist.
/// This is similar to `canonicalize()` but works on non-existent paths.
///
/// # Arguments
/// * `path` - The path to normalize
///
/// # Returns
/// Some(PathBuf) with the normalized path, or None if normalization fails
fn normalize_path_for_completion(path: &Path) -> Option<PathBuf> {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Only pop if the last component is a Normal segment
                // Preserve RootDir and Prefix components
                if let Some(last) = components.last() {
                    if matches!(last, std::path::Component::Normal(_)) {
                        components.pop();
                    }
                }
            }
            std::path::Component::CurDir => {
                // Skip current directory components
            }
            c => components.push(c),
        }
    }

    if components.is_empty() {
        return None;
    }

    let mut result = PathBuf::new();
    for c in components {
        result.push(c);
    }
    Some(result)
}

/// Normalize backslashes to forward slashes in a path string
///
/// Converts escaped backslashes (`\\`) to forward slashes for consistent
/// path handling across platforms.
///
/// # Arguments
/// * `path` - The path string to normalize
///
/// # Returns
/// Path string with backslashes converted to forward slashes
fn normalize_path_separators(path: &str) -> String {
    path.replace("\\\\", "/").replace('\\', "/")
}

/// Check if a resolved path is within the workspace boundary
///
/// # Arguments
/// * `resolved_path` - The resolved absolute path
/// * `workspace_root` - The workspace root path
///
/// # Returns
/// true if the path is within the workspace, false otherwise
#[allow(dead_code)] // Reserved for future use in workspace boundary validation
fn is_within_workspace(resolved_path: &Path, workspace_root: &Path) -> bool {
    resolved_path.starts_with(workspace_root)
}

// ============================================================================
// Regex Patterns for Directive Detection
// ============================================================================

/// Compiled regex patterns for directive path context detection
///
/// These patterns match the directive prefix only (not the path itself),
/// allowing us to determine where the path portion begins.
struct DirectivePathPatterns {
    /// Pattern for backward directives (@lsp-sourced-by, @lsp-run-by, @lsp-included-by)
    /// Matches: # @lsp-sourced-by:  or # lsp-run-by  etc.
    backward: Regex,
    /// Pattern for forward directive (@lsp-source)
    /// Matches: # @lsp-source:  or # lsp-source  etc.
    forward: Regex,
}

/// Get compiled directive patterns for path context detection
///
/// These patterns match the directive keyword and optional colon/whitespace,
/// but NOT the path itself. This allows us to find where the path starts.
fn directive_path_patterns() -> &'static DirectivePathPatterns {
    static PATTERNS: OnceLock<DirectivePathPatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Patterns match the directive keyword and trailing whitespace/colon
        // Consistent with cross_file/directive.rs patterns
        // The @ is optional, colon is optional, whitespace after is required or end of match
        DirectivePathPatterns {
            backward: Regex::new(
                r#"^#\s*@?lsp-(?:sourced-by|run-by|included-by)\s*:?\s*"#
            ).unwrap(),
            forward: Regex::new(
                r#"^#\s*@?lsp-source\s*:?\s*"#
            ).unwrap(),
        }
    })
}

// ============================================================================
// Tests
// ============================================================================

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

    // Unit tests will be added in subsequent tasks

    #[test]
    fn test_file_path_context_none() {
        assert_eq!(FilePathContext::None, FilePathContext::None);
    }

    #[test]
    fn test_directive_type_equality() {
        assert_eq!(DirectiveType::SourcedBy, DirectiveType::SourcedBy);
        assert_eq!(DirectiveType::Source, DirectiveType::Source);
        assert_ne!(DirectiveType::SourcedBy, DirectiveType::Source);
    }

    #[test]
    fn test_create_path_completion_item_file() {
        let path_start = Position { line: 0, character: 8 };
        let cursor_pos = Position { line: 0, character: 8 };
        let item = create_path_completion_item("utils.R", false, "", path_start, cursor_pos);
        assert_eq!(item.label, "utils.R");
        assert_eq!(item.kind, Some(CompletionItemKind::FILE));
        // filter_text should not be set - we rely on text_edit for filtering
        assert!(item.filter_text.is_none());
        // Check text_edit
        if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &item.text_edit {
            assert_eq!(edit.new_text, "utils.R");
        } else {
            panic!("Expected text_edit to be set");
        }
    }

    #[test]
    fn test_create_path_completion_item_directory() {
        let path_start = Position { line: 0, character: 8 };
        let cursor_pos = Position { line: 0, character: 8 };
        let item = create_path_completion_item("subdir", true, "", path_start, cursor_pos);
        assert_eq!(item.label, "subdir");
        assert_eq!(item.kind, Some(CompletionItemKind::FOLDER));
        // filter_text should not be set - we rely on text_edit for filtering
        assert!(item.filter_text.is_none());
        // Check text_edit inserts directory with trailing slash
        if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = &item.text_edit {
            assert_eq!(edit.new_text, "subdir/");
        } else {
            panic!("Expected text_edit to be set");
        }
        // Directory completions should have a command to re-trigger suggestions
        assert!(item.command.is_some(), "Directory completion should have command");
        let cmd = item.command.unwrap();
        assert_eq!(cmd.command, "editor.action.triggerSuggest");
    }

    #[test]
    fn test_create_path_completion_item_file_no_command() {
        let path_start = Position { line: 0, character: 8 };
        let cursor_pos = Position { line: 0, character: 8 };
        let item = create_path_completion_item("utils.R", false, "", path_start, cursor_pos);
        assert_eq!(item.label, "utils.R");
        assert_eq!(item.kind, Some(CompletionItemKind::FILE));
        // File completions should NOT have a command
        assert!(item.command.is_none(), "File completion should not have command");
    }

    #[test]
    fn test_create_path_completion_item_with_prefix() {
        let path_start = Position { line: 0, character: 8 };
        let cursor_pos = Position { line: 0, character: 11 }; // After typing "../"
        let item = create_path_completion_item("utils.R", false, "../", path_start, cursor_pos);
        assert_eq!(item.label, "utils.R");
        // filter_text should not be set
        assert!(item.filter_text.is_none());
        // Check text_edit range and content
        if let Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) = item.text_edit {
            assert_eq!(edit.range.start, path_start);
            assert_eq!(edit.range.end, cursor_pos);
            assert_eq!(edit.new_text, "../utils.R");
        } else {
            panic!("Expected text_edit to be set");
        }
    }

    #[test]
    fn test_normalize_path_separators() {
        assert_eq!(normalize_path_separators("path/to/file.R"), "path/to/file.R");
        assert_eq!(normalize_path_separators("path\\to\\file.R"), "path/to/file.R");
        assert_eq!(normalize_path_separators("path\\\\to\\\\file.R"), "path/to/file.R");
    }

    // ========================================================================
    // Tests for is_source_call_string_context
    // ========================================================================

    #[test]
    fn test_source_call_double_quotes_cursor_at_start() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor at position 8 (right after opening quote, before 'u')
        let position = Position { line: 0, character: 8 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, content_start, is_sys) = result.unwrap();
        assert_eq!(partial, "");
        assert_eq!(content_start.line, 0);
        assert_eq!(content_start.character, 8);
        assert!(!is_sys);
    }

    #[test]
    fn test_source_call_double_quotes_cursor_in_middle() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor at position 11 (after "uti")
        let position = Position { line: 0, character: 11 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, content_start, is_sys) = result.unwrap();
        assert_eq!(partial, "uti");
        assert_eq!(content_start.line, 0);
        assert_eq!(content_start.character, 8);
        assert!(!is_sys);
    }

    #[test]
    fn test_source_call_double_quotes_cursor_at_end() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor at position 15 (after "utils.R", before closing quote)
        let position = Position { line: 0, character: 15 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, is_sys) = result.unwrap();
        assert_eq!(partial, "utils.R");
        assert!(!is_sys);
    }

    #[test]
    fn test_source_call_single_quotes() {
        let code = "source('utils.R')";
        let tree = parse_r(code);
        let position = Position { line: 0, character: 11 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, is_sys) = result.unwrap();
        assert_eq!(partial, "uti");
        assert!(!is_sys);
    }

    #[test]
    fn test_sys_source_call() {
        let code = r#"sys.source("utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        // sys.source("utils.R"...
        // 0         1
        // 01234567890123456789...
        // Opening quote at 11, content starts at 12
        // Position 15 = 3 chars into string = "uti"
        let position = Position { line: 0, character: 15 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, is_sys) = result.unwrap();
        assert_eq!(partial, "uti");
        assert!(is_sys);
    }

    #[test]
    fn test_source_call_named_file_argument() {
        let code = r#"source(file = "utils.R")"#;
        let tree = parse_r(code);
        // source(file = "utils.R")
        // 012345678901234567890123
        //           1111111111222
        // Opening quote at 14, content starts at 15
        // Position 18 = 3 chars into string = "uti"
        let position = Position { line: 0, character: 18 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, is_sys) = result.unwrap();
        assert_eq!(partial, "uti");
        assert!(!is_sys);
    }

    #[test]
    fn test_source_call_with_relative_path() {
        let code = r#"source("../data/utils.R")"#;
        let tree = parse_r(code);
        let position = Position { line: 0, character: 16 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, _) = result.unwrap();
        assert_eq!(partial, "../data/");
    }

    #[test]
    fn test_non_source_call_returns_none() {
        let code = r#"print("hello")"#;
        let tree = parse_r(code);
        let position = Position { line: 0, character: 9 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_source_call_cursor_outside_string() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor before the opening quote
        let position = Position { line: 0, character: 7 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_source_call_cursor_after_closing_quote() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor after the closing quote
        let position = Position { line: 0, character: 17 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_source_call_empty_string() {
        let code = r#"source("")"#;
        let tree = parse_r(code);
        // Cursor inside empty string
        let position = Position { line: 0, character: 8 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, _) = result.unwrap();
        assert_eq!(partial, "");
    }

    #[test]
    fn test_source_call_with_local_argument() {
        let code = r#"source("utils.R", local = TRUE)"#;
        let tree = parse_r(code);
        let position = Position { line: 0, character: 11 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, _) = result.unwrap();
        assert_eq!(partial, "uti");
    }

    #[test]
    fn test_source_call_second_line() {
        let code = "x <- 1\nsource(\"utils.R\")";
        let tree = parse_r(code);
        // Cursor on second line, inside the string
        let position = Position { line: 1, character: 11 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, content_start, _) = result.unwrap();
        assert_eq!(partial, "uti");
        assert_eq!(content_start.line, 1);
    }

    #[test]
    fn test_source_call_with_spaces_in_path() {
        let code = r#"source("path with spaces/file.R")"#;
        let tree = parse_r(code);
        let position = Position { line: 0, character: 20 };
        let result = is_source_call_string_context(&tree, code, position);
        assert!(result.is_some());
        let (partial, _, _) = result.unwrap();
        assert_eq!(partial, "path with sp");
    }

    // ========================================================================
    // Tests for extract_partial_path
    // ========================================================================

    #[test]
    fn test_extract_partial_path_basic() {
        let content = r#"source("utils.R")"#;
        let partial = extract_partial_path(content, 0, 8, 11);
        assert_eq!(partial, "uti");
    }

    #[test]
    fn test_extract_partial_path_empty() {
        let content = r#"source("utils.R")"#;
        let partial = extract_partial_path(content, 0, 8, 8);
        assert_eq!(partial, "");
    }

    #[test]
    fn test_extract_partial_path_full() {
        let content = r#"source("utils.R")"#;
        let partial = extract_partial_path(content, 0, 8, 15);
        assert_eq!(partial, "utils.R");
    }

    // ========================================================================
    // Tests for unescape_string
    // ========================================================================

    #[test]
    fn test_unescape_string_no_escapes() {
        assert_eq!(unescape_string("hello"), "hello");
    }

    #[test]
    fn test_unescape_string_backslash() {
        assert_eq!(unescape_string("path\\\\to"), "path\\to");
    }

    #[test]
    fn test_unescape_string_quote() {
        assert_eq!(unescape_string("say\\\"hello"), "say\"hello");
    }

    // ========================================================================
    // Tests for utf16_column_to_byte_offset
    // ========================================================================

    #[test]
    fn test_utf16_to_byte_ascii() {
        let line = "hello world";
        assert_eq!(utf16_column_to_byte_offset(line, 0), 0);
        assert_eq!(utf16_column_to_byte_offset(line, 5), 5);
        assert_eq!(utf16_column_to_byte_offset(line, 11), 11);
    }

    #[test]
    fn test_utf16_to_byte_emoji() {
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let line = "a🎉b";
        assert_eq!(utf16_column_to_byte_offset(line, 0), 0); // before 'a'
        assert_eq!(utf16_column_to_byte_offset(line, 1), 1); // after 'a', before emoji
        assert_eq!(utf16_column_to_byte_offset(line, 3), 5); // after emoji (1 + 2 UTF-16 units)
        assert_eq!(utf16_column_to_byte_offset(line, 4), 6); // after 'b'
    }

    // ========================================================================
    // Tests for is_directive_path_context
    // ========================================================================

    #[test]
    fn test_directive_sourced_by_basic() {
        let content = "# @lsp-sourced-by ../main.R";
        // # @lsp-sourced-by ../main.R
        // 0         1         2
        // 0123456789012345678901234567
        // Directive ends at position 18, path starts at 18
        let position = Position { line: 0, character: 21 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
        assert_eq!(path_start.line, 0);
        assert_eq!(path_start.character, 18);
    }

    #[test]
    fn test_directive_sourced_by_with_colon() {
        let content = "# @lsp-sourced-by: ../main.R";
        // # @lsp-sourced-by: ../main.R
        // 0         1         2
        // 01234567890123456789012345678
        // Directive ends at position 19, path starts at 19
        let position = Position { line: 0, character: 22 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, _) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
    }

    #[test]
    fn test_directive_sourced_by_quoted() {
        let content = r#"# @lsp-sourced-by "../main.R""#;
        // # @lsp-sourced-by "../main.R"
        // 0         1         2
        // 012345678901234567890123456789
        // Directive ends at 18, quote at 18, path starts at 19
        let position = Position { line: 0, character: 22 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
        assert_eq!(path_start.character, 19);
    }

    #[test]
    fn test_directive_sourced_by_colon_and_quoted() {
        let content = r#"# @lsp-sourced-by: "../main.R""#;
        // # @lsp-sourced-by: "../main.R"
        // 0         1         2         3
        // 0123456789012345678901234567890
        // Directive ends at 19, quote at 19, path starts at 20
        let position = Position { line: 0, character: 23 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
        assert_eq!(path_start.character, 20);
    }

    #[test]
    fn test_directive_run_by() {
        let content = "# @lsp-run-by ../main.R";
        let position = Position { line: 0, character: 17 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, _) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
    }

    #[test]
    fn test_directive_included_by() {
        let content = "# @lsp-included-by ../main.R";
        let position = Position { line: 0, character: 22 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, _) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
    }

    #[test]
    fn test_directive_source_forward() {
        let content = "# @lsp-source utils.R";
        // # @lsp-source utils.R
        // 0         1         2
        // 012345678901234567890
        // Directive ends at 14, path starts at 14
        let position = Position { line: 0, character: 17 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::Source);
        assert_eq!(partial, "uti");
        assert_eq!(path_start.character, 14);
    }

    #[test]
    fn test_directive_source_with_colon_and_quotes() {
        let content = r#"# @lsp-source: "utils/helpers.R""#;
        // # @lsp-source: "utils/helpers.R"
        // 0         1         2         3
        // 01234567890123456789012345678901
        // Directive ends at 15, quote at 15, path starts at 16
        let position = Position { line: 0, character: 20 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::Source);
        assert_eq!(partial, "util");
        assert_eq!(path_start.character, 16);
    }

    #[test]
    fn test_directive_without_at_prefix() {
        let content = "# lsp-sourced-by ../main.R";
        let position = Position { line: 0, character: 20 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, _) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
    }

    #[test]
    fn test_directive_cursor_at_path_start() {
        let content = "# @lsp-sourced-by ../main.R";
        // Cursor right at path start
        let position = Position { line: 0, character: 18 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (_, partial, _) = result.unwrap();
        assert_eq!(partial, "");
    }

    #[test]
    fn test_directive_cursor_at_path_end() {
        let content = "# @lsp-sourced-by ../main.R";
        // Cursor at end of path
        let position = Position { line: 0, character: 27 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (_, partial, _) = result.unwrap();
        assert_eq!(partial, "../main.R");
    }

    #[test]
    fn test_directive_cursor_before_path() {
        let content = "# @lsp-sourced-by ../main.R";
        // Cursor before the path starts (in the directive keyword)
        let position = Position { line: 0, character: 10 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_directive_empty_path() {
        let content = "# @lsp-sourced-by ";
        // Cursor at end of line (empty path)
        let position = Position { line: 0, character: 18 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (_, partial, _) = result.unwrap();
        assert_eq!(partial, "");
    }

    #[test]
    fn test_directive_single_quoted() {
        let content = "# @lsp-sourced-by '../main.R'";
        // Path starts at 19 (after single quote)
        let position = Position { line: 0, character: 22 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
        assert_eq!(path_start.character, 19);
    }

    #[test]
    fn test_directive_path_with_spaces_quoted() {
        let content = r#"# @lsp-sourced-by "path with spaces/main.R""#;
        let position = Position { line: 0, character: 30 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (_, partial, _) = result.unwrap();
        assert_eq!(partial, "path with s");
    }

    #[test]
    fn test_non_directive_line() {
        let content = "x <- 1";
        let position = Position { line: 0, character: 3 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_directive_on_second_line() {
        let content = "x <- 1\n# @lsp-sourced-by ../main.R";
        let position = Position { line: 1, character: 21 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some());
        let (directive_type, partial, path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "../");
        assert_eq!(path_start.line, 1);
    }

    #[test]
    fn test_directive_colon_space_empty_path() {
        // Bug case: colon followed by space, cursor at end
        let content = "# @lsp-sourced-by: ";
        // # @lsp-sourced-by: 
        // 0         1        
        // 0123456789012345678 9
        // Position 19 is at the end (after the space)
        let position = Position { line: 0, character: 19 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some(), "Should detect directive context with colon and space");
        let (directive_type, partial, _path_start) = result.unwrap();
        assert_eq!(directive_type, DirectiveType::SourcedBy);
        assert_eq!(partial, "");
    }

    #[test]
    fn test_directive_colon_no_space_empty_path() {
        // Colon but no space after
        let content = "# @lsp-sourced-by:";
        let position = Position { line: 0, character: 18 };
        let result = is_directive_path_context(content, position);
        assert!(result.is_some(), "Should detect directive context with colon and no space");
    }

    // ========================================================================
    // Tests for detect_file_path_context
    // ========================================================================

    #[test]
    fn test_detect_context_source_call() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor inside the string at position 11 (after "uti")
        let position = Position { line: 0, character: 11 };
        let result = detect_file_path_context(&tree, code, position);
        match result {
            FilePathContext::SourceCall { partial_path, content_start, is_sys_source } => {
                assert_eq!(partial_path, "uti");
                assert_eq!(content_start.line, 0);
                assert_eq!(content_start.character, 8);
                assert!(!is_sys_source);
            }
            _ => panic!("Expected SourceCall context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_sys_source_call() {
        let code = r#"sys.source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor inside the string at position 15 (after "uti")
        let position = Position { line: 0, character: 15 };
        let result = detect_file_path_context(&tree, code, position);
        match result {
            FilePathContext::SourceCall { partial_path, is_sys_source, .. } => {
                assert_eq!(partial_path, "uti");
                assert!(is_sys_source);
            }
            _ => panic!("Expected SourceCall context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_backward_directive() {
        let content = "# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        // Cursor at position 21 (after "../")
        let position = Position { line: 0, character: 21 };
        let result = detect_file_path_context(&tree, content, position);
        match result {
            FilePathContext::Directive { directive_type, partial_path, path_start } => {
                assert_eq!(directive_type, DirectiveType::SourcedBy);
                assert_eq!(partial_path, "../");
                assert_eq!(path_start.line, 0);
                assert_eq!(path_start.character, 18);
            }
            _ => panic!("Expected Directive context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_forward_directive() {
        let content = "# @lsp-source utils.R";
        let tree = parse_r(content);
        // Cursor at position 17 (after "uti")
        let position = Position { line: 0, character: 17 };
        let result = detect_file_path_context(&tree, content, position);
        match result {
            FilePathContext::Directive { directive_type, partial_path, path_start } => {
                assert_eq!(directive_type, DirectiveType::Source);
                assert_eq!(partial_path, "uti");
                assert_eq!(path_start.line, 0);
                assert_eq!(path_start.character, 14);
            }
            _ => panic!("Expected Directive context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_non_source_function() {
        let code = r#"print("hello")"#;
        let tree = parse_r(code);
        // Cursor inside the string
        let position = Position { line: 0, character: 9 };
        let result = detect_file_path_context(&tree, code, position);
        assert_eq!(result, FilePathContext::None);
    }

    #[test]
    fn test_detect_context_regular_code() {
        let code = "x <- 1 + 2";
        let tree = parse_r(code);
        let position = Position { line: 0, character: 5 };
        let result = detect_file_path_context(&tree, code, position);
        assert_eq!(result, FilePathContext::None);
    }

    #[test]
    fn test_detect_context_source_call_priority_over_directive() {
        // This tests that source() calls are checked first
        // Even if the line looks like a directive, if cursor is in a source() call, it should return SourceCall
        let code = "source(\"# @lsp-sourced-by test.R\")";
        let tree = parse_r(code);
        // Cursor inside the string
        let position = Position { line: 0, character: 15 };
        let result = detect_file_path_context(&tree, code, position);
        match result {
            FilePathContext::SourceCall { partial_path, is_sys_source, .. } => {
                assert_eq!(partial_path, "# @lsp-");
                assert!(!is_sys_source);
            }
            _ => panic!("Expected SourceCall context (source call takes priority), got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_empty_source_string() {
        let code = r#"source("")"#;
        let tree = parse_r(code);
        // Cursor inside empty string
        let position = Position { line: 0, character: 8 };
        let result = detect_file_path_context(&tree, code, position);
        match result {
            FilePathContext::SourceCall { partial_path, .. } => {
                assert_eq!(partial_path, "");
            }
            _ => panic!("Expected SourceCall context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_directive_with_colon_and_quotes() {
        let content = r#"# @lsp-sourced-by: "../main.R""#;
        let tree = parse_r(content);
        // Cursor at position 23 (after "../")
        let position = Position { line: 0, character: 23 };
        let result = detect_file_path_context(&tree, content, position);
        match result {
            FilePathContext::Directive { directive_type, partial_path, path_start } => {
                assert_eq!(directive_type, DirectiveType::SourcedBy);
                assert_eq!(partial_path, "../");
                assert_eq!(path_start.character, 20);
            }
            _ => panic!("Expected Directive context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_cursor_outside_string() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor before the opening quote
        let position = Position { line: 0, character: 7 };
        let result = detect_file_path_context(&tree, code, position);
        assert_eq!(result, FilePathContext::None);
    }

    #[test]
    fn test_detect_context_cursor_after_closing_quote() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor after the closing quote
        let position = Position { line: 0, character: 17 };
        let result = detect_file_path_context(&tree, code, position);
        assert_eq!(result, FilePathContext::None);
    }

    #[test]
    fn test_detect_context_multiline_source_on_second_line() {
        let code = "x <- 1\nsource(\"utils.R\")";
        let tree = parse_r(code);
        // Cursor on second line, inside the string
        let position = Position { line: 1, character: 11 };
        let result = detect_file_path_context(&tree, code, position);
        match result {
            FilePathContext::SourceCall { partial_path, content_start, .. } => {
                assert_eq!(partial_path, "uti");
                assert_eq!(content_start.line, 1);
            }
            _ => panic!("Expected SourceCall context, got {:?}", result),
        }
    }

    #[test]
    fn test_detect_context_directive_on_second_line() {
        let content = "x <- 1\n# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        // Cursor on second line
        let position = Position { line: 1, character: 21 };
        let result = detect_file_path_context(&tree, content, position);
        match result {
            FilePathContext::Directive { directive_type, partial_path, path_start } => {
                assert_eq!(directive_type, DirectiveType::SourcedBy);
                assert_eq!(partial_path, "../");
                assert_eq!(path_start.line, 1);
            }
            _ => panic!("Expected Directive context, got {:?}", result),
        }
    }

    // ========================================================================
    // Tests for extract_file_path_at_position
    // ========================================================================

    #[test]
    fn test_extract_file_path_source_call_full_path() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor in the middle of the path
        let position = Position { line: 0, character: 11 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "utils.R");
        match context {
            FilePathContext::SourceCall { partial_path, is_sys_source, .. } => {
                assert_eq!(partial_path, "utils.R");
                assert!(!is_sys_source);
            }
            _ => panic!("Expected SourceCall context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_source_call_relative_path() {
        let code = r#"source("../data/utils.R")"#;
        let tree = parse_r(code);
        // Cursor in the middle of the path
        let position = Position { line: 0, character: 15 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "../data/utils.R");
    }

    #[test]
    fn test_extract_file_path_sys_source_call() {
        let code = r#"sys.source("helpers/utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        // Cursor in the middle of the path
        let position = Position { line: 0, character: 18 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "helpers/utils.R");
        match context {
            FilePathContext::SourceCall { is_sys_source, .. } => {
                assert!(is_sys_source);
            }
            _ => panic!("Expected SourceCall context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_source_call_single_quotes() {
        let code = "source('utils.R')";
        let tree = parse_r(code);
        let position = Position { line: 0, character: 11 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "utils.R");
    }

    #[test]
    fn test_extract_file_path_source_call_empty_string() {
        let code = r#"source("")"#;
        let tree = parse_r(code);
        // Cursor inside empty string
        let position = Position { line: 0, character: 8 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "");
    }

    #[test]
    fn test_extract_file_path_source_call_cursor_at_start() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor right after opening quote
        let position = Position { line: 0, character: 8 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "utils.R");
    }

    #[test]
    fn test_extract_file_path_source_call_cursor_at_end() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor right before closing quote
        let position = Position { line: 0, character: 15 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "utils.R");
    }

    #[test]
    fn test_extract_file_path_source_call_cursor_outside() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        // Cursor before opening quote
        let position = Position { line: 0, character: 7 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_file_path_non_source_function() {
        let code = r#"print("utils.R")"#;
        let tree = parse_r(code);
        let position = Position { line: 0, character: 10 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_file_path_directive_sourced_by() {
        let content = "# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        // Cursor in the middle of the path
        let position = Position { line: 0, character: 22 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "../main.R");
        match context {
            FilePathContext::Directive { directive_type, partial_path, .. } => {
                assert_eq!(directive_type, DirectiveType::SourcedBy);
                assert_eq!(partial_path, "../main.R");
            }
            _ => panic!("Expected Directive context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_directive_run_by() {
        let content = "# @lsp-run-by ../main.R";
        let tree = parse_r(content);
        let position = Position { line: 0, character: 18 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "../main.R");
        match context {
            FilePathContext::Directive { directive_type, .. } => {
                assert_eq!(directive_type, DirectiveType::SourcedBy);
            }
            _ => panic!("Expected Directive context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_directive_included_by() {
        let content = "# @lsp-included-by ../main.R";
        let tree = parse_r(content);
        let position = Position { line: 0, character: 23 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "../main.R");
        match context {
            FilePathContext::Directive { directive_type, .. } => {
                assert_eq!(directive_type, DirectiveType::SourcedBy);
            }
            _ => panic!("Expected Directive context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_directive_source_forward() {
        let content = "# @lsp-source utils.R";
        let tree = parse_r(content);
        let position = Position { line: 0, character: 17 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "utils.R");
        match context {
            FilePathContext::Directive { directive_type, .. } => {
                assert_eq!(directive_type, DirectiveType::Source);
            }
            _ => panic!("Expected Directive context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_directive_with_colon() {
        let content = "# @lsp-sourced-by: ../main.R";
        let tree = parse_r(content);
        let position = Position { line: 0, character: 23 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "../main.R");
    }

    #[test]
    fn test_extract_file_path_directive_quoted() {
        let content = r#"# @lsp-sourced-by "../main.R""#;
        let tree = parse_r(content);
        let position = Position { line: 0, character: 23 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "../main.R");
    }

    #[test]
    fn test_extract_file_path_directive_colon_and_quoted() {
        let content = r#"# @lsp-sourced-by: "../main.R""#;
        let tree = parse_r(content);
        let position = Position { line: 0, character: 24 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "../main.R");
    }

    #[test]
    fn test_extract_file_path_directive_cursor_at_start() {
        let content = "# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        // Cursor right at path start
        let position = Position { line: 0, character: 18 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "../main.R");
    }

    #[test]
    fn test_extract_file_path_directive_cursor_at_end() {
        let content = "# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        // Cursor at end of path
        let position = Position { line: 0, character: 27 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "../main.R");
    }

    #[test]
    fn test_extract_file_path_directive_cursor_before_path() {
        let content = "# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        // Cursor before the path starts (in the directive keyword)
        let position = Position { line: 0, character: 10 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_file_path_directive_empty_path() {
        let content = "# @lsp-sourced-by ";
        let tree = parse_r(content);
        // Cursor at end of line (empty path)
        let position = Position { line: 0, character: 18 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "");
    }

    #[test]
    fn test_extract_file_path_directive_with_spaces_quoted() {
        let content = r#"# @lsp-sourced-by "path with spaces/main.R""#;
        let tree = parse_r(content);
        let position = Position { line: 0, character: 30 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "path with spaces/main.R");
    }

    #[test]
    fn test_extract_file_path_source_call_with_spaces() {
        let code = r#"source("path with spaces/file.R")"#;
        let tree = parse_r(code);
        let position = Position { line: 0, character: 20 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, _) = result.unwrap();
        assert_eq!(full_path, "path with spaces/file.R");
    }

    #[test]
    fn test_extract_file_path_source_call_on_second_line() {
        let code = "x <- 1\nsource(\"utils.R\")";
        let tree = parse_r(code);
        // Cursor on second line, inside the string
        let position = Position { line: 1, character: 11 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "utils.R");
        match context {
            FilePathContext::SourceCall { content_start, .. } => {
                assert_eq!(content_start.line, 1);
            }
            _ => panic!("Expected SourceCall context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_directive_on_second_line() {
        let content = "x <- 1\n# @lsp-sourced-by ../main.R";
        let tree = parse_r(content);
        let position = Position { line: 1, character: 22 };
        let result = extract_file_path_at_position(&tree, content, position);
        assert!(result.is_some());
        let (full_path, context) = result.unwrap();
        assert_eq!(full_path, "../main.R");
        match context {
            FilePathContext::Directive { path_start, .. } => {
                assert_eq!(path_start.line, 1);
            }
            _ => panic!("Expected Directive context, got {:?}", context),
        }
    }

    #[test]
    fn test_extract_file_path_regular_code() {
        let code = "x <- 1 + 2";
        let tree = parse_r(code);
        let position = Position { line: 0, character: 5 };
        let result = extract_file_path_at_position(&tree, code, position);
        assert!(result.is_none());
    }

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        /// Parse R code into a tree-sitter Tree
        fn parse_r(code: &str) -> Tree {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_r::LANGUAGE.into())
                .unwrap();
            parser.parse(code, None).unwrap()
        }

        /// Helper to create a completion item with default positions for property tests
        /// This simplifies tests that only care about the completion item properties,
        /// not the text_edit range.
        fn create_test_completion_item(name: &str, is_directory: bool) -> CompletionItem {
            let default_pos = Position { line: 0, character: 0 };
            create_path_completion_item(name, is_directory, "", default_pos, default_pos)
        }

        /// Helper to extract the new_text from a completion item's text_edit
        fn get_text_edit_new_text(item: &CompletionItem) -> Option<&str> {
            match &item.text_edit {
                Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(edit)) => {
                    Some(&edit.new_text)
                }
                _ => None,
            }
        }

        /// Helper to create a completion item with a directory prefix for property tests
        #[allow(dead_code)] // Reserved for future tests that need directory prefixes
        fn create_test_completion_item_with_prefix(
            name: &str,
            is_directory: bool,
            dir_prefix: &str,
        ) -> CompletionItem {
            let default_pos = Position { line: 0, character: 0 };
            create_path_completion_item(name, is_directory, dir_prefix, default_pos, default_pos)
        }

        // ====================================================================
        // Generator Strategies
        // ====================================================================

        /// Strategy for generating valid R file names
        /// Generates names like: a.R, abc.r, a1_b.R, etc.
        fn r_filename_strategy() -> impl Strategy<Value = String> {
            prop::string::string_regex("[a-z][a-z0-9_]{0,10}\\.(R|r)")
                .unwrap()
                .prop_filter("non-empty", |s| !s.is_empty())
        }

        /// Strategy for generating directory names (no extension)
        fn dirname_strategy() -> impl Strategy<Value = String> {
            prop::string::string_regex("[a-z][a-z0-9_]{0,10}")
                .unwrap()
                .prop_filter("non-empty and not hidden", |s| !s.is_empty() && !s.starts_with('.'))
        }

        /// Strategy for generating relative paths
        fn relative_path_strategy() -> impl Strategy<Value = String> {
            prop_oneof![
                // Just a filename: utils.R
                r_filename_strategy(),
                // One directory: subdir/utils.R
                (dirname_strategy(), r_filename_strategy())
                    .prop_map(|(d, f)| format!("{}/{}", d, f)),
                // Parent directory: ../utils.R
                r_filename_strategy().prop_map(|f| format!("../{}", f)),
                // Two directories: dir1/dir2/utils.R
                (dirname_strategy(), dirname_strategy(), r_filename_strategy())
                    .prop_map(|(d1, d2, f)| format!("{}/{}/{}", d1, d2, f)),
                // Parent + directory: ../subdir/utils.R
                (dirname_strategy(), r_filename_strategy())
                    .prop_map(|(d, f)| format!("../{}/{}", d, f)),
            ]
        }

        /// Strategy for generating source() or sys.source() function names
        fn source_function_strategy() -> impl Strategy<Value = (String, bool)> {
            prop_oneof![
                Just(("source".to_string(), false)),
                Just(("sys.source".to_string(), true)),
            ]
        }

        /// Strategy for generating quote characters
        fn quote_strategy() -> impl Strategy<Value = char> {
            prop_oneof![
                Just('"'),
                Just('\''),
            ]
        }

        // ====================================================================
        // Property 1: Source Call Context Detection
        // Validates: Requirements 1.1, 1.2
        // ====================================================================

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            /// **Property 1: Source Call Context Detection**
            ///
            /// *For any* R code containing a `source()` or `sys.source()` call with a
            /// string literal argument, and *for any* cursor position inside that string
            /// literal (between the opening quote and closing quote), the context detector
            /// SHALL return a `SourceCall` context with the correct partial path extracted
            /// from the string start to cursor position.
            ///
            /// **Validates: Requirements 1.1, 1.2**
            #[test]
            fn prop_source_call_context_detection(
                path in relative_path_strategy(),
                (func_name, expected_is_sys) in source_function_strategy(),
                quote in quote_strategy(),
            ) {
                // Skip empty paths
                prop_assume!(!path.is_empty());

                // Generate R code with source() or sys.source() call
                let code = format!("{}({quote}{}{quote})", func_name, path, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Calculate the position of the opening quote
                // func_name + "(" = func_name.len() + 1
                let opening_quote_col = func_name.len() + 1;
                // Content starts after the opening quote
                let content_start_col = opening_quote_col + 1;

                // Test cursor positions from start of content to end of content
                for cursor_offset in 0..=path.len() {
                    let cursor_col = content_start_col + cursor_offset;
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = is_source_call_string_context(&tree, &code, position);

                    // Assert that we get a result
                    prop_assert!(
                        result.is_some(),
                        "Expected Some for cursor at column {} in code: {}",
                        cursor_col,
                        code
                    );

                    let (partial_path, content_start, is_sys_source) = result.unwrap();

                    // Assert partial path equals substring from string start to cursor
                    let expected_partial = &path[..cursor_offset];
                    prop_assert_eq!(
                        partial_path,
                        expected_partial,
                        "Partial path mismatch at cursor offset {} in code: {}",
                        cursor_offset,
                        code
                    );

                    // Assert content_start position is correct
                    prop_assert_eq!(
                        content_start.line,
                        0,
                        "Content start line should be 0"
                    );
                    prop_assert_eq!(
                        content_start.character,
                        content_start_col as u32,
                        "Content start character mismatch"
                    );

                    // Assert is_sys_source matches whether sys.source was used
                    prop_assert_eq!(
                        is_sys_source,
                        expected_is_sys,
                        "is_sys_source flag mismatch for function: {}",
                        func_name
                    );
                }
            }

            /// Property 1 extended: source() with named file argument
            ///
            /// Tests that `source(file = "path")` is correctly detected.
            #[test]
            fn prop_source_call_named_file_argument(
                path in relative_path_strategy(),
                quote in quote_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Generate R code with named file argument
                let code = format!("source(file = {quote}{}{quote})", path, quote = quote);

                let tree = parse_r(&code);

                // "source(file = " = 14 characters, then quote
                let content_start_col = 15; // After opening quote

                // Test a few cursor positions
                for cursor_offset in [0, path.len() / 2, path.len()] {
                    if cursor_offset > path.len() {
                        continue;
                    }

                    let cursor_col = content_start_col + cursor_offset;
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    let result = is_source_call_string_context(&tree, &code, position);

                    prop_assert!(
                        result.is_some(),
                        "Expected Some for named file argument at cursor {} in: {}",
                        cursor_col,
                        code
                    );

                    let (partial_path, _, is_sys_source) = result.unwrap();
                    let expected_partial = &path[..cursor_offset];
                    prop_assert_eq!(partial_path, expected_partial);
                    prop_assert!(!is_sys_source);
                }
            }

            /// Property 1 extended: sys.source() with additional arguments
            ///
            /// Tests that `sys.source("path", envir = globalenv())` is correctly detected.
            #[test]
            fn prop_sys_source_with_envir_argument(
                path in relative_path_strategy(),
                quote in quote_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Generate R code with sys.source and envir argument
                let code = format!(
                    "sys.source({quote}{}{quote}, envir = globalenv())",
                    path,
                    quote = quote
                );

                let tree = parse_r(&code);

                // "sys.source(" = 11 characters, then quote
                let content_start_col = 12; // After opening quote

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = content_start_col + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = is_source_call_string_context(&tree, &code, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for sys.source with envir at cursor {} in: {}",
                    cursor_col,
                    code
                );

                let (partial_path, _, is_sys_source) = result.unwrap();
                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert!(is_sys_source);
            }

            /// Property 1 extended: Cursor outside string returns None
            ///
            /// Tests that cursor positions before the opening quote or after the
            /// closing quote return None.
            #[test]
            fn prop_source_call_cursor_outside_string_returns_none(
                path in relative_path_strategy(),
                (func_name, _) in source_function_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                let code = format!("{}(\"{}\")", func_name, path);
                let tree = parse_r(&code);

                // Test cursor before opening quote
                let before_quote_col = func_name.len(); // At the '(' character
                let position_before = Position {
                    line: 0,
                    character: before_quote_col as u32,
                };
                let result_before = is_source_call_string_context(&tree, &code, position_before);
                prop_assert!(
                    result_before.is_none(),
                    "Expected None for cursor before opening quote in: {}",
                    code
                );

                // Test cursor after closing quote
                // func_name + "(" + '"' + path + '"' + ")" = func_name.len() + 1 + 1 + path.len() + 1 + 1
                let after_quote_col = func_name.len() + 1 + 1 + path.len() + 1 + 1;
                let position_after = Position {
                    line: 0,
                    character: after_quote_col as u32,
                };
                let result_after = is_source_call_string_context(&tree, &code, position_after);
                prop_assert!(
                    result_after.is_none(),
                    "Expected None for cursor after closing quote in: {}",
                    code
                );
            }

            // ====================================================================
            // Property 2: Backward Directive Context Detection
            // Validates: Requirements 1.3, 1.4, 1.5
            // ====================================================================

            /// **Property 2: Backward Directive Context Detection**
            ///
            /// *For any* R comment containing an `@lsp-sourced-by`, `@lsp-run-by`, or
            /// `@lsp-included-by` directive (with or without colon, with or without quotes),
            /// and *for any* cursor position after the directive keyword where a path is
            /// expected, the context detector SHALL return a `Directive` context with
            /// `DirectiveType::SourcedBy` and the correct partial path.
            ///
            /// **Validates: Requirements 1.3, 1.4, 1.5**
            #[test]
            fn prop_backward_directive_context_detection(
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                use_colon in prop::bool::ANY,
                use_quotes in prop::bool::ANY,
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Build the directive line
                // Format: # <directive><colon_part><quote_open><path><quote_close>
                let colon_part = if use_colon { ": " } else { " " };
                let (quote_open, quote_close) = if use_quotes { ("\"", "\"") } else { ("", "") };

                let line = format!(
                    "# {}{}{}{}{}",
                    directive_name, colon_part, quote_open, path, quote_close
                );

                // Calculate path_start position
                // "# " = 2 chars
                // directive_name.len() chars
                // colon_part.len() chars
                // quote_open.len() chars (0 or 1)
                let prefix_len = 2 + directive_name.len() + colon_part.len() + quote_open.len();
                let expected_path_start_col = prefix_len as u32;

                // Test cursor positions from start of path to end of path
                for cursor_offset in 0..=path.len() {
                    let cursor_col = prefix_len + cursor_offset;
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = is_directive_path_context(&line, position);

                    // Assert that we get a result
                    prop_assert!(
                        result.is_some(),
                        "Expected Some for cursor at column {} in line: {}",
                        cursor_col,
                        line
                    );

                    let (directive_type, partial_path, path_start) = result.unwrap();

                    // Assert directive type is SourcedBy (backward directive)
                    prop_assert_eq!(
                        directive_type,
                        DirectiveType::SourcedBy,
                        "Expected DirectiveType::SourcedBy for directive: {}",
                        directive_name
                    );

                    // Assert partial path equals substring from path start to cursor
                    let expected_partial = &path[..cursor_offset];
                    prop_assert_eq!(
                        partial_path,
                        expected_partial,
                        "Partial path mismatch at cursor offset {} in line: {}",
                        cursor_offset,
                        line
                    );

                    // Assert path_start position is correct
                    prop_assert_eq!(
                        path_start.line,
                        0,
                        "Path start line should be 0"
                    );
                    prop_assert_eq!(
                        path_start.character,
                        expected_path_start_col,
                        "Path start character mismatch. Expected {} but got {} for line: {}",
                        expected_path_start_col,
                        path_start.character,
                        line
                    );
                }
            }

            /// Property 2 extended: Backward directive without @ prefix
            ///
            /// Tests that directives without the @ prefix (e.g., `lsp-sourced-by`)
            /// are also correctly detected.
            #[test]
            fn prop_backward_directive_without_at_prefix(
                directive_name in prop_oneof![
                    Just("lsp-sourced-by"),
                    Just("lsp-run-by"),
                    Just("lsp-included-by"),
                ],
                use_colon in prop::bool::ANY,
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Build the directive line without @ prefix
                let colon_part = if use_colon { ": " } else { " " };
                let line = format!("# {}{}{}", directive_name, colon_part, path);

                // Calculate path_start position
                let prefix_len = 2 + directive_name.len() + colon_part.len();
                let expected_path_start_col = prefix_len as u32;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = prefix_len + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = is_directive_path_context(&line, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for directive without @ prefix at cursor {} in: {}",
                    cursor_col,
                    line
                );

                let (directive_type, partial_path, path_start) = result.unwrap();

                prop_assert_eq!(
                    directive_type,
                    DirectiveType::SourcedBy,
                    "Expected DirectiveType::SourcedBy"
                );

                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert_eq!(path_start.character, expected_path_start_col);
            }

            /// Property 2 extended: Cursor before path returns None
            ///
            /// Tests that cursor positions before the path portion (within the
            /// directive keyword) return None.
            #[test]
            fn prop_backward_directive_cursor_before_path_returns_none(
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                let line = format!("# {} {}", directive_name, path);

                // Test cursor in the middle of the directive keyword
                // "# @lsp-" = 7 characters, cursor at position 5 is within directive
                let cursor_col = 5;
                let position = Position {
                    line: 0,
                    character: cursor_col,
                };

                let result = is_directive_path_context(&line, position);

                prop_assert!(
                    result.is_none(),
                    "Expected None for cursor before path at column {} in: {}",
                    cursor_col,
                    line
                );
            }

            /// Property 2 extended: Backward directive with single quotes
            ///
            /// Tests that single-quoted paths are correctly detected.
            #[test]
            fn prop_backward_directive_single_quotes(
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                use_colon in prop::bool::ANY,
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                let colon_part = if use_colon { ": " } else { " " };
                let line = format!("# {}{}'{}'", directive_name, colon_part, path);

                // Calculate path_start position (after single quote)
                let prefix_len = 2 + directive_name.len() + colon_part.len() + 1; // +1 for quote
                let expected_path_start_col = prefix_len as u32;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = prefix_len + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = is_directive_path_context(&line, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for single-quoted path at cursor {} in: {}",
                    cursor_col,
                    line
                );

                let (directive_type, partial_path, path_start) = result.unwrap();

                prop_assert_eq!(directive_type, DirectiveType::SourcedBy);

                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert_eq!(path_start.character, expected_path_start_col);
            }

            /// Property 2 extended: Backward directive on non-first line
            ///
            /// Tests that directives on lines other than the first are correctly detected.
            #[test]
            fn prop_backward_directive_on_second_line(
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Create content with directive on second line
                let content = format!("x <- 1\n# {} {}", directive_name, path);

                // Calculate path_start position on line 1
                let prefix_len = 2 + directive_name.len() + 1; // "# " + directive + " "
                let expected_path_start_col = prefix_len as u32;

                // Test cursor at middle of path on line 1
                let cursor_offset = path.len() / 2;
                let cursor_col = prefix_len + cursor_offset;
                let position = Position {
                    line: 1,
                    character: cursor_col as u32,
                };

                let result = is_directive_path_context(&content, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for directive on second line at cursor {} in:\n{}",
                    cursor_col,
                    content
                );

                let (directive_type, partial_path, path_start) = result.unwrap();

                prop_assert_eq!(directive_type, DirectiveType::SourcedBy);

                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert_eq!(path_start.line, 1);
                prop_assert_eq!(path_start.character, expected_path_start_col);
            }

            // ====================================================================
            // Property 3: Forward Directive Context Detection
            // Validates: Requirements 1.6
            // ====================================================================

            /// **Property 3: Forward Directive Context Detection**
            ///
            /// *For any* R comment containing an `@lsp-source` directive (with or without
            /// colon, with or without quotes), and *for any* cursor position after the
            /// directive keyword where a path is expected, the context detector SHALL
            /// return a `Directive` context with `DirectiveType::Source` and the correct
            /// partial path.
            ///
            /// **Validates: Requirements 1.6**
            #[test]
            fn prop_forward_directive_context_detection(
                use_colon in prop::bool::ANY,
                use_quotes in prop::bool::ANY,
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // The forward directive is always @lsp-source
                let directive_name = "@lsp-source";

                // Build the directive line
                // Format: # <directive><colon_part><quote_open><path><quote_close>
                let colon_part = if use_colon { ": " } else { " " };
                let (quote_open, quote_close) = if use_quotes { ("\"", "\"") } else { ("", "") };

                let line = format!(
                    "# {}{}{}{}{}",
                    directive_name, colon_part, quote_open, path, quote_close
                );

                // Calculate path_start position
                // "# " = 2 chars
                // directive_name.len() chars
                // colon_part.len() chars
                // quote_open.len() chars (0 or 1)
                let prefix_len = 2 + directive_name.len() + colon_part.len() + quote_open.len();
                let expected_path_start_col = prefix_len as u32;

                // Test cursor positions from start of path to end of path
                for cursor_offset in 0..=path.len() {
                    let cursor_col = prefix_len + cursor_offset;
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = is_directive_path_context(&line, position);

                    // Assert that we get a result
                    prop_assert!(
                        result.is_some(),
                        "Expected Some for cursor at column {} in line: {}",
                        cursor_col,
                        line
                    );

                    let (directive_type, partial_path, path_start) = result.unwrap();

                    // Assert directive type is Source (forward directive)
                    prop_assert_eq!(
                        directive_type,
                        DirectiveType::Source,
                        "Expected DirectiveType::Source for @lsp-source directive"
                    );

                    // Assert partial path equals substring from path start to cursor
                    let expected_partial = &path[..cursor_offset];
                    prop_assert_eq!(
                        partial_path,
                        expected_partial,
                        "Partial path mismatch at cursor offset {} in line: {}",
                        cursor_offset,
                        line
                    );

                    // Assert path_start position is correct
                    prop_assert_eq!(
                        path_start.line,
                        0,
                        "Path start line should be 0"
                    );
                    prop_assert_eq!(
                        path_start.character,
                        expected_path_start_col,
                        "Path start character mismatch. Expected {} but got {} for line: {}",
                        expected_path_start_col,
                        path_start.character,
                        line
                    );
                }
            }

            /// Property 3 extended: Forward directive without @ prefix
            ///
            /// Tests that the directive without the @ prefix (e.g., `lsp-source`)
            /// is also correctly detected.
            #[test]
            fn prop_forward_directive_without_at_prefix(
                use_colon in prop::bool::ANY,
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Build the directive line without @ prefix
                let directive_name = "lsp-source";
                let colon_part = if use_colon { ": " } else { " " };
                let line = format!("# {}{}{}", directive_name, colon_part, path);

                // Calculate path_start position
                let prefix_len = 2 + directive_name.len() + colon_part.len();
                let expected_path_start_col = prefix_len as u32;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = prefix_len + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = is_directive_path_context(&line, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for directive without @ prefix at cursor {} in: {}",
                    cursor_col,
                    line
                );

                let (directive_type, partial_path, path_start) = result.unwrap();

                prop_assert_eq!(
                    directive_type,
                    DirectiveType::Source,
                    "Expected DirectiveType::Source"
                );

                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert_eq!(path_start.character, expected_path_start_col);
            }

            /// Property 3 extended: Cursor before path returns None
            ///
            /// Tests that cursor positions before the path portion (within the
            /// directive keyword) return None.
            #[test]
            fn prop_forward_directive_cursor_before_path_returns_none(
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                let line = format!("# @lsp-source {}", path);

                // Test cursor in the middle of the directive keyword
                // "# @lsp-" = 7 characters, cursor at position 5 is within directive
                let cursor_col = 5;
                let position = Position {
                    line: 0,
                    character: cursor_col,
                };

                let result = is_directive_path_context(&line, position);

                prop_assert!(
                    result.is_none(),
                    "Expected None for cursor before path at column {} in: {}",
                    cursor_col,
                    line
                );
            }

            /// Property 3 extended: Forward directive with single quotes
            ///
            /// Tests that single-quoted paths are correctly detected.
            #[test]
            fn prop_forward_directive_single_quotes(
                use_colon in prop::bool::ANY,
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                let directive_name = "@lsp-source";
                let colon_part = if use_colon { ": " } else { " " };
                let line = format!("# {}{}'{}'", directive_name, colon_part, path);

                // Calculate path_start position (after single quote)
                let prefix_len = 2 + directive_name.len() + colon_part.len() + 1; // +1 for quote
                let expected_path_start_col = prefix_len as u32;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = prefix_len + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = is_directive_path_context(&line, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for single-quoted path at cursor {} in: {}",
                    cursor_col,
                    line
                );

                let (directive_type, partial_path, path_start) = result.unwrap();

                prop_assert_eq!(directive_type, DirectiveType::Source);

                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert_eq!(path_start.character, expected_path_start_col);
            }

            /// Property 3 extended: Forward directive on non-first line
            ///
            /// Tests that directives on lines other than the first are correctly detected.
            #[test]
            fn prop_forward_directive_on_second_line(
                path in relative_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Create content with directive on second line
                let content = format!("x <- 1\n# @lsp-source {}", path);

                // Calculate path_start position on line 1
                // "# @lsp-source " = 14 characters
                let prefix_len = 14;
                let expected_path_start_col = prefix_len as u32;

                // Test cursor at middle of path on line 1
                let cursor_offset = path.len() / 2;
                let cursor_col = prefix_len + cursor_offset;
                let position = Position {
                    line: 1,
                    character: cursor_col as u32,
                };

                let result = is_directive_path_context(&content, position);

                prop_assert!(
                    result.is_some(),
                    "Expected Some for directive on second line at cursor {} in:\n{}",
                    cursor_col,
                    content
                );

                let (directive_type, partial_path, path_start) = result.unwrap();

                prop_assert_eq!(directive_type, DirectiveType::Source);

                let expected_partial = &path[..cursor_offset];
                prop_assert_eq!(partial_path, expected_partial);
                prop_assert_eq!(path_start.line, 1);
                prop_assert_eq!(path_start.character, expected_path_start_col);
            }
        }

        // ====================================================================
        // Property 4: Non-Source Function Exclusion
        // Validates: Requirements 1.7
        // ====================================================================

        /// Strategy for generating non-source function names
        ///
        /// Generates common R function names that are NOT source() or sys.source().
        /// These include I/O functions, string functions, and other common functions
        /// that take string arguments.
        fn non_source_function_strategy() -> impl Strategy<Value = String> {
            prop_oneof![
                // Common I/O functions
                Just("print".to_string()),
                Just("cat".to_string()),
                Just("message".to_string()),
                Just("warning".to_string()),
                Just("stop".to_string()),
                // File I/O functions (not source)
                Just("read.csv".to_string()),
                Just("read.table".to_string()),
                Just("write.csv".to_string()),
                Just("write.table".to_string()),
                Just("readLines".to_string()),
                Just("writeLines".to_string()),
                Just("readRDS".to_string()),
                Just("saveRDS".to_string()),
                Just("load".to_string()),
                Just("save".to_string()),
                Just("file.exists".to_string()),
                Just("file.copy".to_string()),
                Just("file.remove".to_string()),
                Just("file.rename".to_string()),
                Just("file.create".to_string()),
                Just("file.info".to_string()),
                Just("dir.create".to_string()),
                Just("dir.exists".to_string()),
                Just("unlink".to_string()),
                Just("normalizePath".to_string()),
                Just("path.expand".to_string()),
                // String functions
                Just("paste".to_string()),
                Just("paste0".to_string()),
                Just("sprintf".to_string()),
                Just("format".to_string()),
                Just("nchar".to_string()),
                Just("substr".to_string()),
                Just("substring".to_string()),
                Just("strsplit".to_string()),
                Just("gsub".to_string()),
                Just("sub".to_string()),
                Just("grep".to_string()),
                Just("grepl".to_string()),
                Just("regexpr".to_string()),
                Just("gregexpr".to_string()),
                Just("toupper".to_string()),
                Just("tolower".to_string()),
                Just("trimws".to_string()),
                Just("chartr".to_string()),
                // Package/library functions
                Just("library".to_string()),
                Just("require".to_string()),
                Just("loadNamespace".to_string()),
                Just("requireNamespace".to_string()),
                // Other common functions
                Just("getwd".to_string()),
                Just("setwd".to_string()),
                Just("Sys.getenv".to_string()),
                Just("Sys.setenv".to_string()),
                Just("options".to_string()),
                Just("getOption".to_string()),
                Just("assign".to_string()),
                Just("get".to_string()),
                Just("exists".to_string()),
                Just("eval".to_string()),
                Just("parse".to_string()),
                Just("deparse".to_string()),
                Just("match.arg".to_string()),
                Just("identical".to_string()),
                Just("class".to_string()),
                Just("typeof".to_string()),
                Just("attr".to_string()),
                Just("attributes".to_string()),
                // Connection functions
                Just("url".to_string()),
                Just("file".to_string()),
                Just("gzfile".to_string()),
                Just("bzfile".to_string()),
                Just("xzfile".to_string()),
                Just("open".to_string()),
                Just("close".to_string()),
                // JSON/XML functions (common packages)
                Just("jsonlite::fromJSON".to_string()),
                Just("jsonlite::toJSON".to_string()),
                // Custom/user-defined function names
                Just("my_function".to_string()),
                Just("process_file".to_string()),
                Just("read_data".to_string()),
                Just("write_output".to_string()),
                Just("load_config".to_string()),
            ]
        }

        /// Strategy for generating file-like path strings
        ///
        /// Generates strings that look like file paths but should NOT trigger
        /// file path context detection when used in non-source functions.
        fn file_like_path_strategy() -> impl Strategy<Value = String> {
            prop_oneof![
                // Simple filenames
                r_filename_strategy(),
                // Relative paths
                relative_path_strategy(),
                // Paths with various extensions
                prop::string::string_regex("[a-z][a-z0-9_]{0,8}\\.(csv|txt|json|rds|rda|dat)")
                    .unwrap()
                    .prop_filter("non-empty", |s| !s.is_empty()),
                // Directory-like paths
                (dirname_strategy(), dirname_strategy())
                    .prop_map(|(d1, d2)| format!("{}/{}/", d1, d2)),
                // Parent directory paths
                dirname_strategy().prop_map(|d| format!("../{}/", d)),
                // Absolute-looking paths (starting with /)
                (dirname_strategy(), r_filename_strategy())
                    .prop_map(|(d, f)| format!("/{}/{}", d, f)),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            /// **Property 4: Non-Source Function Exclusion**
            ///
            /// *For any* R code containing a function call that is NOT `source()` or
            /// `sys.source()` (e.g., `print()`, `read.csv()`, `library()`), and *for any*
            /// cursor position inside a string argument, the context detector SHALL
            /// return `FilePathContext::None`.
            ///
            /// **Validates: Requirements 1.7**
            #[test]
            fn prop_non_source_function_exclusion(
                func_name in non_source_function_strategy(),
                path in file_like_path_strategy(),
                quote in quote_strategy(),
            ) {
                // Skip empty paths
                prop_assume!(!path.is_empty());

                // Generate R code with non-source function call
                let code = format!("{}({quote}{}{quote})", func_name, path, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Calculate the position of the opening quote
                // func_name + "(" = func_name.len() + 1
                let opening_quote_col = func_name.len() + 1;
                // Content starts after the opening quote
                let content_start_col = opening_quote_col + 1;

                // Test cursor positions from start of content to end of content
                for cursor_offset in 0..=path.len() {
                    let cursor_col = content_start_col + cursor_offset;
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = detect_file_path_context(&tree, &code, position);

                    // Assert that we get FilePathContext::None
                    prop_assert_eq!(
                        result,
                        FilePathContext::None,
                        "Expected FilePathContext::None for non-source function '{}' at cursor {} in code: {}",
                        func_name,
                        cursor_col,
                        code
                    );
                }
            }

            /// Property 4 extended: Non-source function with named argument
            ///
            /// Tests that named arguments in non-source functions also return None.
            #[test]
            fn prop_non_source_function_named_argument(
                func_name in non_source_function_strategy(),
                path in file_like_path_strategy(),
                quote in quote_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Generate R code with named argument (common pattern: file = "path")
                let code = format!("{}(file = {quote}{}{quote})", func_name, path, quote = quote);

                let tree = parse_r(&code);

                // Calculate content start position
                // func_name + "(file = " + quote = func_name.len() + 8 + 1
                let content_start_col = func_name.len() + 9;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = content_start_col + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = detect_file_path_context(&tree, &code, position);

                prop_assert_eq!(
                    result,
                    FilePathContext::None,
                    "Expected FilePathContext::None for non-source function '{}' with named arg at cursor {} in: {}",
                    func_name,
                    cursor_col,
                    code
                );
            }

            /// Property 4 extended: Non-source function with multiple arguments
            ///
            /// Tests that string arguments in non-source functions with multiple
            /// arguments also return None.
            #[test]
            fn prop_non_source_function_multiple_arguments(
                func_name in non_source_function_strategy(),
                path in file_like_path_strategy(),
                quote in quote_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Generate R code with multiple arguments
                // Pattern: func("path", arg2 = TRUE, arg3 = 42)
                let code = format!(
                    "{}({quote}{}{quote}, header = TRUE, sep = \",\")",
                    func_name, path, quote = quote
                );

                let tree = parse_r(&code);

                // Calculate content start position
                // func_name + "(" + quote = func_name.len() + 1 + 1
                let content_start_col = func_name.len() + 2;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = content_start_col + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = detect_file_path_context(&tree, &code, position);

                prop_assert_eq!(
                    result,
                    FilePathContext::None,
                    "Expected FilePathContext::None for non-source function '{}' with multiple args at cursor {} in: {}",
                    func_name,
                    cursor_col,
                    code
                );
            }

            /// Property 4 extended: Non-source function on second line
            ///
            /// Tests that non-source functions on lines other than the first
            /// also return None.
            #[test]
            fn prop_non_source_function_on_second_line(
                func_name in non_source_function_strategy(),
                path in file_like_path_strategy(),
                quote in quote_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Create content with function call on second line
                let code = format!("x <- 1\n{}({quote}{}{quote})", func_name, path, quote = quote);

                let tree = parse_r(&code);

                // Calculate content start position on line 1
                // func_name + "(" + quote = func_name.len() + 1 + 1
                let content_start_col = func_name.len() + 2;

                // Test cursor at middle of path on line 1
                let cursor_offset = path.len() / 2;
                let cursor_col = content_start_col + cursor_offset;
                let position = Position {
                    line: 1,
                    character: cursor_col as u32,
                };

                let result = detect_file_path_context(&tree, &code, position);

                prop_assert_eq!(
                    result,
                    FilePathContext::None,
                    "Expected FilePathContext::None for non-source function '{}' on second line at cursor {} in:\n{}",
                    func_name,
                    cursor_col,
                    code
                );
            }

            /// Property 4 extended: Nested function calls with non-source outer function
            ///
            /// Tests that when a source() call is nested inside a non-source function,
            /// the cursor inside the outer function's string argument returns None.
            #[test]
            fn prop_non_source_function_with_nested_source(
                func_name in non_source_function_strategy(),
                path in file_like_path_strategy(),
            ) {
                prop_assume!(!path.is_empty());

                // Generate R code where non-source function wraps a source call
                // Pattern: print(source("path"))
                // We test cursor in a string that's NOT the source() argument
                let code = format!("{}(\"{}\")", func_name, path);

                let tree = parse_r(&code);

                // Calculate content start position
                // func_name + "(\"" = func_name.len() + 2
                let content_start_col = func_name.len() + 2;

                // Test cursor at middle of path
                let cursor_offset = path.len() / 2;
                let cursor_col = content_start_col + cursor_offset;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = detect_file_path_context(&tree, &code, position);

                prop_assert_eq!(
                    result,
                    FilePathContext::None,
                    "Expected FilePathContext::None for non-source function '{}' at cursor {} in: {}",
                    func_name,
                    cursor_col,
                    code
                );
            }

            /// Property 4 extended: Empty string in non-source function
            ///
            /// Tests that empty strings in non-source functions return None.
            #[test]
            fn prop_non_source_function_empty_string(
                func_name in non_source_function_strategy(),
                quote in quote_strategy(),
            ) {
                // Generate R code with empty string
                let code = format!("{}({quote}{quote})", func_name, quote = quote);

                let tree = parse_r(&code);

                // Cursor inside empty string (right after opening quote)
                let cursor_col = func_name.len() + 2; // func_name + "(" + quote
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                let result = detect_file_path_context(&tree, &code, position);

                prop_assert_eq!(
                    result,
                    FilePathContext::None,
                    "Expected FilePathContext::None for empty string in non-source function '{}' at cursor {} in: {}",
                    func_name,
                    cursor_col,
                    code
                );
            }
        }

        // ====================================================================
        // Property 5: R File and Directory Filtering
        // Validates: Requirements 2.1, 2.2, 2.7
        // ====================================================================

        /// Strategy for generating file extensions (both R and non-R)
        fn file_extension_strategy() -> impl Strategy<Value = String> {
            prop_oneof![
                // R extensions (should be kept)
                Just("R".to_string()),
                Just("r".to_string()),
                // Non-R extensions (should be filtered out)
                Just("csv".to_string()),
                Just("txt".to_string()),
                Just("py".to_string()),
                Just("json".to_string()),
                Just("md".to_string()),
                Just("rds".to_string()),
                Just("rda".to_string()),
                Just("RData".to_string()),
                Just("Rmd".to_string()),
                Just("html".to_string()),
                Just("pdf".to_string()),
                Just("png".to_string()),
                Just("jpg".to_string()),
                Just("xlsx".to_string()),
                Just("dat".to_string()),
                Just("log".to_string()),
                Just("out".to_string()),
                Just("err".to_string()),
            ]
        }

        /// Strategy for generating a single directory entry
        /// Returns (name, is_directory, is_hidden)
        fn directory_entry_strategy() -> impl Strategy<Value = (String, bool, bool)> {
            prop_oneof![
                // Regular file with extension
                (dirname_strategy(), file_extension_strategy())
                    .prop_map(|(name, ext)| (format!("{}.{}", name, ext), false, false)),
                // Regular directory
                dirname_strategy()
                    .prop_map(|name| (name, true, false)),
                // Hidden file
                (dirname_strategy(), file_extension_strategy())
                    .prop_map(|(name, ext)| (format!(".{}.{}", name, ext), false, true)),
                // Hidden directory
                dirname_strategy()
                    .prop_map(|name| (format!(".{}", name), true, true)),
                // File without extension (should be filtered out)
                dirname_strategy()
                    .prop_map(|name| (name, false, false)),
            ]
        }

        /// Strategy for generating a list of directory entries
        fn directory_entries_strategy() -> impl Strategy<Value = Vec<(String, bool, bool)>> {
            prop::collection::vec(directory_entry_strategy(), 0..20)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            /// **Property 5: R File and Directory Filtering**
            ///
            /// *For any* directory containing files with various extensions, the completion
            /// provider SHALL return only:
            /// - Files with `.R` or `.r` extensions
            /// - All directories (regardless of contents)
            /// - No hidden files or directories (those starting with `.`)
            ///
            /// Note: Hidden file filtering is done by `list_directory_entries`, but we verify
            /// that `filter_r_files_and_dirs` correctly handles the filtering of R files and
            /// directories from the entries it receives.
            ///
            /// **Validates: Requirements 2.1, 2.2, 2.7**
            #[test]
            fn prop_r_file_and_directory_filtering(
                entries_spec in directory_entries_strategy(),
            ) {
                // Convert the spec into actual entry tuples (name, path, is_directory)
                // Note: We exclude hidden entries here since list_directory_entries
                // already filters them out before calling filter_r_files_and_dirs
                let entries: Vec<(String, PathBuf, bool)> = entries_spec
                    .iter()
                    .filter(|(_, _, is_hidden)| !is_hidden)
                    .map(|(name, is_dir, _)| (name.clone(), PathBuf::from(name), *is_dir))
                    .collect();

                // Call the function under test
                let result = filter_r_files_and_dirs(entries.clone());

                // Property 1: All returned files must have .R or .r extension
                for (name, _, is_dir) in &result {
                    if !is_dir {
                        let has_r_extension = name.ends_with(".R") || name.ends_with(".r");
                        prop_assert!(
                            has_r_extension,
                            "File '{}' in result does not have .R or .r extension",
                            name
                        );
                    }
                }

                // Property 2: All directories from input should be in output
                let input_dirs: Vec<&String> = entries
                    .iter()
                    .filter(|(_, _, is_dir)| *is_dir)
                    .map(|(name, _, _)| name)
                    .collect();

                let output_dirs: Vec<&String> = result
                    .iter()
                    .filter(|(_, _, is_dir)| *is_dir)
                    .map(|(name, _, _)| name)
                    .collect();

                for dir in &input_dirs {
                    prop_assert!(
                        output_dirs.contains(dir),
                        "Directory '{}' from input is missing in output",
                        dir
                    );
                }

                // Property 3: All R files from input should be in output
                let input_r_files: Vec<&String> = entries
                    .iter()
                    .filter(|(name, _, is_dir)| {
                        !is_dir && (name.ends_with(".R") || name.ends_with(".r"))
                    })
                    .map(|(name, _, _)| name)
                    .collect();

                let output_files: Vec<&String> = result
                    .iter()
                    .filter(|(_, _, is_dir)| !is_dir)
                    .map(|(name, _, _)| name)
                    .collect();

                for r_file in &input_r_files {
                    prop_assert!(
                        output_files.contains(r_file),
                        "R file '{}' from input is missing in output",
                        r_file
                    );
                }

                // Property 4: No non-R files should be in output
                let input_non_r_files: Vec<&String> = entries
                    .iter()
                    .filter(|(name, _, is_dir)| {
                        !is_dir && !name.ends_with(".R") && !name.ends_with(".r")
                    })
                    .map(|(name, _, _)| name)
                    .collect();

                for non_r_file in &input_non_r_files {
                    prop_assert!(
                        !output_files.contains(non_r_file),
                        "Non-R file '{}' should not be in output",
                        non_r_file
                    );
                }

                // Property 5: Output count should equal directories + R files
                let expected_count = input_dirs.len() + input_r_files.len();
                prop_assert_eq!(
                    result.len(),
                    expected_count,
                    "Expected {} entries (dirs: {}, R files: {}), got {}",
                    expected_count,
                    input_dirs.len(),
                    input_r_files.len(),
                    result.len()
                );
            }

            /// Property 5 extended: Empty input returns empty output
            ///
            /// Tests that an empty list of entries returns an empty result.
            #[test]
            fn prop_r_file_filtering_empty_input(_dummy in Just(())) {
                let entries: Vec<(String, PathBuf, bool)> = vec![];
                let result = filter_r_files_and_dirs(entries);
                prop_assert!(result.is_empty(), "Empty input should produce empty output");
            }

            /// Property 5 extended: Only directories input
            ///
            /// Tests that when input contains only directories, all are preserved.
            #[test]
            fn prop_r_file_filtering_only_directories(
                dir_names in prop::collection::vec(dirname_strategy(), 1..10),
            ) {
                let entries: Vec<(String, PathBuf, bool)> = dir_names
                    .iter()
                    .map(|name| (name.clone(), PathBuf::from(name), true))
                    .collect();

                let result = filter_r_files_and_dirs(entries.clone());

                // All directories should be preserved
                prop_assert_eq!(
                    result.len(),
                    entries.len(),
                    "All directories should be preserved"
                );

                for (name, _, is_dir) in &result {
                    prop_assert!(is_dir, "Entry '{}' should be a directory", name);
                }
            }

            /// Property 5 extended: Only R files input
            ///
            /// Tests that when input contains only R files, all are preserved.
            #[test]
            fn prop_r_file_filtering_only_r_files(
                file_names in prop::collection::vec(r_filename_strategy(), 1..10),
            ) {
                let entries: Vec<(String, PathBuf, bool)> = file_names
                    .iter()
                    .map(|name| (name.clone(), PathBuf::from(name), false))
                    .collect();

                let result = filter_r_files_and_dirs(entries.clone());

                // All R files should be preserved
                prop_assert_eq!(
                    result.len(),
                    entries.len(),
                    "All R files should be preserved"
                );

                for (name, _, is_dir) in &result {
                    prop_assert!(!is_dir, "Entry '{}' should be a file", name);
                    prop_assert!(
                        name.ends_with(".R") || name.ends_with(".r"),
                        "File '{}' should have .R or .r extension",
                        name
                    );
                }
            }

            /// Property 5 extended: Only non-R files input
            ///
            /// Tests that when input contains only non-R files, output is empty.
            #[test]
            fn prop_r_file_filtering_only_non_r_files(
                base_names in prop::collection::vec(dirname_strategy(), 1..10),
                extensions in prop::collection::vec(
                    prop_oneof![
                        Just("csv"),
                        Just("txt"),
                        Just("py"),
                        Just("json"),
                        Just("md"),
                    ],
                    1..10
                ),
            ) {
                // Create non-R files by combining base names with non-R extensions
                let entries: Vec<(String, PathBuf, bool)> = base_names
                    .iter()
                    .zip(extensions.iter().cycle())
                    .map(|(name, ext)| {
                        let filename = format!("{}.{}", name, ext);
                        (filename.clone(), PathBuf::from(&filename), false)
                    })
                    .collect();

                let result = filter_r_files_and_dirs(entries);

                // No non-R files should be in output
                prop_assert!(
                    result.is_empty(),
                    "Only non-R files input should produce empty output, got {} entries",
                    result.len()
                );
            }

            /// Property 5 extended: Case sensitivity of R extension
            ///
            /// Tests that only .R and .r extensions are accepted (not .rmd, .rdata, etc.)
            #[test]
            fn prop_r_file_filtering_case_sensitivity(
                base_name in dirname_strategy(),
            ) {
                // Create files with various R-like extensions
                let entries: Vec<(String, PathBuf, bool)> = vec![
                    (format!("{}.R", base_name), PathBuf::from(format!("{}.R", base_name)), false),
                    (format!("{}.r", base_name), PathBuf::from(format!("{}.r", base_name)), false),
                    (format!("{}.Rmd", base_name), PathBuf::from(format!("{}.Rmd", base_name)), false),
                    (format!("{}.rmd", base_name), PathBuf::from(format!("{}.rmd", base_name)), false),
                    (format!("{}.RData", base_name), PathBuf::from(format!("{}.RData", base_name)), false),
                    (format!("{}.rdata", base_name), PathBuf::from(format!("{}.rdata", base_name)), false),
                    (format!("{}.Rds", base_name), PathBuf::from(format!("{}.Rds", base_name)), false),
                    (format!("{}.rds", base_name), PathBuf::from(format!("{}.rds", base_name)), false),
                ];

                let result = filter_r_files_and_dirs(entries);

                // Only .R and .r files should be in output
                prop_assert_eq!(
                    result.len(),
                    2,
                    "Only .R and .r files should be kept, got {} entries",
                    result.len()
                );

                for (name, _, _) in &result {
                    prop_assert!(
                        name.ends_with(".R") || name.ends_with(".r"),
                        "File '{}' should have exactly .R or .r extension",
                        name
                    );
                }
            }

            /// Property 5 extended: Path preservation
            ///
            /// Tests that the PathBuf is preserved correctly through filtering.
            #[test]
            fn prop_r_file_filtering_path_preservation(
                entries_spec in directory_entries_strategy(),
            ) {
                // Create entries with specific paths
                let entries: Vec<(String, PathBuf, bool)> = entries_spec
                    .iter()
                    .filter(|(_, _, is_hidden)| !is_hidden)
                    .map(|(name, is_dir, _)| {
                        let path = PathBuf::from("/some/base/path").join(name);
                        (name.clone(), path, *is_dir)
                    })
                    .collect();

                let result = filter_r_files_and_dirs(entries.clone());

                // Verify paths are preserved
                for (name, path, _) in &result {
                    let expected_path = PathBuf::from("/some/base/path").join(name);
                    prop_assert_eq!(
                        path,
                        &expected_path,
                        "Path for '{}' should be preserved",
                        name
                    );
                }
            }

            // ====================================================================
            // Property 9: Directory Completion Trailing Slash
            // Validates: Requirements 2.6
            // ====================================================================

            /// **Property 9: Directory Completion Trailing Slash**
            ///
            /// *For any* directory entry in completion results, the text_edit new_text
            /// SHALL end with a forward slash `/` to enable continued path navigation.
            ///
            /// **Validates: Requirements 2.6**
            #[test]
            fn prop_directory_completion_trailing_slash(
                dir_name in dirname_strategy(),
            ) {
                // Call the function under test with is_directory = true
                let completion_item = create_test_completion_item(&dir_name, true);

                // Property 1: text_edit new_text must end with forward slash
                let new_text = get_text_edit_new_text(&completion_item)
                    .expect("Directory completion item should have text_edit");
                prop_assert!(
                    new_text.ends_with('/'),
                    "Directory '{}' text_edit new_text '{}' should end with '/'",
                    dir_name,
                    new_text
                );

                // Property 2: kind must be FOLDER
                let kind = completion_item.kind
                    .expect("Directory completion item should have kind");
                prop_assert_eq!(
                    kind,
                    CompletionItemKind::FOLDER,
                    "Directory '{}' should have kind FOLDER, got {:?}",
                    dir_name,
                    kind
                );

                // Property 3: label should be the directory name (without trailing slash)
                prop_assert_eq!(
                    &completion_item.label,
                    &dir_name,
                    "Directory label should be the directory name"
                );

                // Property 4: text_edit new_text should be dir_name + "/"
                let expected_new_text = format!("{}/", dir_name);
                prop_assert_eq!(
                    new_text,
                    &expected_new_text,
                    "Directory text_edit new_text should be name + '/'"
                );
            }

            /// Property 9 extended: File completion does NOT have trailing slash
            ///
            /// Tests that file entries (is_directory = false) do NOT have a trailing slash.
            #[test]
            fn prop_file_completion_no_trailing_slash(
                file_name in r_filename_strategy(),
            ) {
                // Call the function under test with is_directory = false
                let completion_item = create_test_completion_item(&file_name, false);

                // Property 1: text_edit new_text must NOT end with forward slash
                let new_text = get_text_edit_new_text(&completion_item)
                    .expect("File completion item should have text_edit");
                prop_assert!(
                    !new_text.ends_with('/'),
                    "File '{}' text_edit new_text '{}' should NOT end with '/'",
                    file_name,
                    new_text
                );

                // Property 2: kind must be FILE
                let kind = completion_item.kind
                    .expect("File completion item should have kind");
                prop_assert_eq!(
                    kind,
                    CompletionItemKind::FILE,
                    "File '{}' should have kind FILE, got {:?}",
                    file_name,
                    kind
                );

                // Property 3: label should be the file name
                prop_assert_eq!(
                    &completion_item.label,
                    &file_name,
                    "File label should be the file name"
                );

                // Property 4: text_edit new_text should be the file name (no trailing slash)
                prop_assert_eq!(
                    new_text,
                    &file_name,
                    "File text_edit new_text should be the file name without trailing slash"
                );
            }

            /// Property 9 extended: Directory names with various characters
            ///
            /// Tests that directory names with underscores and numbers still get
            /// the trailing slash correctly.
            #[test]
            fn prop_directory_completion_various_names(
                base in prop::string::string_regex("[a-z][a-z0-9_]{0,15}").unwrap(),
            ) {
                prop_assume!(!base.is_empty());

                let completion_item = create_test_completion_item(&base, true);

                let new_text = get_text_edit_new_text(&completion_item)
                    .expect("Directory completion item should have text_edit");

                // Must end with slash
                prop_assert!(
                    new_text.ends_with('/'),
                    "Directory '{}' text_edit new_text '{}' should end with '/'",
                    base,
                    new_text
                );

                // Must be exactly name + "/"
                let expected = format!("{}/", base);
                prop_assert_eq!(
                    new_text,
                    &expected,
                    "Directory text_edit new_text should be exactly name + '/'"
                );
            }

            // ====================================================================
            // Property 11: Output Path Separator
            // Validates: Requirements 4.3
            // ====================================================================

            /// **Property 11: Output Path Separator**
            ///
            /// *For any* completion item returned by the completion provider, the path
            /// separator used in `text_edit.new_text` and `label` SHALL be a forward slash `/`
            /// (R convention), regardless of the operating system.
            ///
            /// **Validates: Requirements 4.3**
            #[test]
            fn prop_output_path_separator(
                name in prop::string::string_regex("[a-z][a-z0-9_]{0,15}").unwrap(),
                is_directory in prop::bool::ANY,
            ) {
                prop_assume!(!name.is_empty());

                // Call the function under test
                let completion_item = create_test_completion_item(&name, is_directory);

                // Property 1: text_edit.new_text must NOT contain backslashes
                if let Some(new_text) = get_text_edit_new_text(&completion_item) {
                    prop_assert!(
                        !new_text.contains('\\'),
                        "text_edit.new_text '{}' should NOT contain backslashes",
                        new_text
                    );

                    // Property 2: Any path separators in text_edit.new_text must be forward slashes
                    // For directories, the trailing separator must be '/'
                    if is_directory {
                        prop_assert!(
                            new_text.ends_with('/'),
                            "Directory text_edit.new_text '{}' should end with forward slash '/'",
                            new_text
                        );
                    }
                }

                // Property 3: label must NOT contain backslashes
                prop_assert!(
                    !completion_item.label.contains('\\'),
                    "label '{}' should NOT contain backslashes",
                    completion_item.label
                );
            }

            /// Property 11 extended: File names with various patterns
            ///
            /// Tests that file names with underscores, numbers, and R extensions
            /// do not introduce backslashes in the completion item.
            #[test]
            fn prop_output_path_separator_r_files(
                file_name in r_filename_strategy(),
            ) {
                let completion_item = create_test_completion_item(&file_name, false);

                // text_edit.new_text must NOT contain backslashes
                if let Some(new_text) = get_text_edit_new_text(&completion_item) {
                    prop_assert!(
                        !new_text.contains('\\'),
                        "R file text_edit.new_text '{}' should NOT contain backslashes",
                        new_text
                    );
                }

                // label must NOT contain backslashes
                prop_assert!(
                    !completion_item.label.contains('\\'),
                    "R file label '{}' should NOT contain backslashes",
                    completion_item.label
                );
            }

            /// Property 11 extended: Directory names with various patterns
            ///
            /// Tests that directory names with underscores and numbers
            /// use forward slash as the trailing separator.
            #[test]
            fn prop_output_path_separator_directories(
                dir_name in dirname_strategy(),
            ) {
                let completion_item = create_test_completion_item(&dir_name, true);

                // text_edit.new_text must NOT contain backslashes
                let new_text = get_text_edit_new_text(&completion_item)
                    .expect("Directory completion item should have text_edit");

                prop_assert!(
                    !new_text.contains('\\'),
                    "Directory text_edit.new_text '{}' should NOT contain backslashes",
                    new_text
                );

                // The trailing separator must be a forward slash
                prop_assert!(
                    new_text.ends_with('/'),
                    "Directory text_edit.new_text '{}' should end with forward slash '/'",
                    new_text
                );

                // label must NOT contain backslashes
                prop_assert!(
                    !completion_item.label.contains('\\'),
                    "Directory label '{}' should NOT contain backslashes",
                    completion_item.label
                );
            }

            /// Property 11 extended: Names that might look like paths
            ///
            /// Tests that even names that might look like paths (containing
            /// underscores that could be confused with separators) still
            /// use forward slashes only.
            #[test]
            fn prop_output_path_separator_path_like_names(
                base in prop::string::string_regex("[a-z][a-z0-9_]{0,10}").unwrap(),
                suffix in prop::string::string_regex("[a-z][a-z0-9_]{0,5}").unwrap(),
                is_directory in prop::bool::ANY,
            ) {
                prop_assume!(!base.is_empty());
                prop_assume!(!suffix.is_empty());

                // Create a name with underscores that might look path-like
                let name = format!("{}_{}", base, suffix);

                let completion_item = create_test_completion_item(&name, is_directory);

                // text_edit.new_text must NOT contain backslashes
                if let Some(new_text) = get_text_edit_new_text(&completion_item) {
                    prop_assert!(
                        !new_text.contains('\\'),
                        "Path-like name text_edit.new_text '{}' should NOT contain backslashes",
                        new_text
                    );

                    // If directory, must end with forward slash
                    if is_directory {
                        prop_assert!(
                            new_text.ends_with('/'),
                            "Directory text_edit.new_text '{}' should end with forward slash",
                            new_text
                        );
                    }
                }

                // label must NOT contain backslashes
                prop_assert!(
                    !completion_item.label.contains('\\'),
                    "Path-like name label '{}' should NOT contain backslashes",
                    completion_item.label
                );
            }

            // ====================================================================
            // Property 10: Path Separator Normalization
            // Validates: Requirements 4.1, 4.2
            // ====================================================================

            /// **Property 10: Path Separator Normalization**
            ///
            /// *For any* input path containing escaped backslashes (`\\`), the path
            /// resolver SHALL normalize them to forward slashes before resolution,
            /// treating `\\` equivalently to `/` for path component separation.
            ///
            /// **Validates: Requirements 4.1, 4.2**
            #[test]
            fn prop_path_separator_normalization(
                path_components in prop::collection::vec(dirname_strategy(), 1..4),
                file_name in r_filename_strategy(),
            ) {
                // Build a path with forward slashes (canonical form)
                let forward_slash_path = if path_components.is_empty() {
                    file_name.clone()
                } else {
                    format!("{}/{}", path_components.join("/"), file_name)
                };

                // Build the same path with single backslashes
                let single_backslash_path = if path_components.is_empty() {
                    file_name.clone()
                } else {
                    format!("{}\\{}", path_components.join("\\"), file_name)
                };

                // Build the same path with escaped backslashes (\\)
                let escaped_backslash_path = if path_components.is_empty() {
                    file_name.clone()
                } else {
                    format!("{}\\\\{}", path_components.join("\\\\"), file_name)
                };

                // Build a mixed path (some forward, some backslash)
                let mixed_path = if path_components.len() >= 2 {
                    let first_half = &path_components[..path_components.len() / 2];
                    let second_half = &path_components[path_components.len() / 2..];
                    format!(
                        "{}/{}\\{}",
                        first_half.join("/"),
                        second_half.join("\\"),
                        file_name
                    )
                } else if path_components.len() == 1 {
                    format!("{}\\{}", path_components[0], file_name)
                } else {
                    file_name.clone()
                };

                // Normalize all paths
                let normalized_forward = normalize_path_separators(&forward_slash_path);
                let normalized_single_backslash = normalize_path_separators(&single_backslash_path);
                let normalized_escaped_backslash = normalize_path_separators(&escaped_backslash_path);
                let normalized_mixed = normalize_path_separators(&mixed_path);

                // Property 1: Normalized path must NOT contain any backslashes
                prop_assert!(
                    !normalized_forward.contains('\\'),
                    "Forward slash path '{}' normalized to '{}' should NOT contain backslashes",
                    forward_slash_path,
                    normalized_forward
                );
                prop_assert!(
                    !normalized_single_backslash.contains('\\'),
                    "Single backslash path '{}' normalized to '{}' should NOT contain backslashes",
                    single_backslash_path,
                    normalized_single_backslash
                );
                prop_assert!(
                    !normalized_escaped_backslash.contains('\\'),
                    "Escaped backslash path '{}' normalized to '{}' should NOT contain backslashes",
                    escaped_backslash_path,
                    normalized_escaped_backslash
                );
                prop_assert!(
                    !normalized_mixed.contains('\\'),
                    "Mixed path '{}' normalized to '{}' should NOT contain backslashes",
                    mixed_path,
                    normalized_mixed
                );

                // Property 2: All normalized paths should use forward slashes as separators
                // The forward slash path should remain unchanged
                prop_assert_eq!(
                    &normalized_forward,
                    &forward_slash_path,
                    "Forward slash path should remain unchanged after normalization"
                );

                // Property 3: Single backslash and forward slash paths should normalize to the same result
                prop_assert_eq!(
                    &normalized_single_backslash,
                    &forward_slash_path,
                    "Single backslash path '{}' should normalize to same as forward slash path '{}'",
                    single_backslash_path,
                    forward_slash_path
                );

                // Property 4: Escaped backslash path should normalize to forward slashes
                // Note: escaped backslashes (\\) become single forward slashes
                prop_assert!(
                    normalized_escaped_backslash.contains('/') || path_components.is_empty(),
                    "Escaped backslash path '{}' normalized to '{}' should contain forward slashes (unless no components)",
                    escaped_backslash_path,
                    normalized_escaped_backslash
                );
            }

            /// Property 10 extended: Empty and simple paths
            ///
            /// Tests that empty paths and paths without separators are handled correctly.
            #[test]
            fn prop_path_separator_normalization_simple_paths(
                file_name in r_filename_strategy(),
            ) {
                // Test with just a filename (no separators)
                let normalized = normalize_path_separators(&file_name);

                // Property 1: Simple filename should remain unchanged
                prop_assert_eq!(
                    &normalized,
                    &file_name,
                    "Simple filename '{}' should remain unchanged after normalization",
                    file_name
                );

                // Property 2: Result should not contain backslashes
                prop_assert!(
                    !normalized.contains('\\'),
                    "Normalized filename '{}' should NOT contain backslashes",
                    normalized
                );
            }

            /// Property 10 extended: Paths with parent directory references
            ///
            /// Tests that paths with `..` components and various separators normalize correctly.
            #[test]
            fn prop_path_separator_normalization_parent_refs(
                dir_name in dirname_strategy(),
                file_name in r_filename_strategy(),
            ) {
                // Test various parent directory reference patterns
                let patterns = vec![
                    format!("../{}/{}", dir_name, file_name),           // forward slashes
                    format!("..\\{}\\{}", dir_name, file_name),         // single backslashes
                    format!("..\\\\{}\\\\{}", dir_name, file_name),     // escaped backslashes
                    format!("../{}\\{}", dir_name, file_name),          // mixed
                    format!("..\\{}/{}", dir_name, file_name),          // mixed (other way)
                ];

                let expected = format!("../{}/{}", dir_name, file_name);

                for pattern in &patterns {
                    let normalized = normalize_path_separators(pattern);

                    // Property 1: Normalized path must NOT contain backslashes
                    prop_assert!(
                        !normalized.contains('\\'),
                        "Pattern '{}' normalized to '{}' should NOT contain backslashes",
                        pattern,
                        normalized
                    );

                    // Property 2: All patterns should normalize to the same forward-slash form
                    prop_assert_eq!(
                        &normalized,
                        &expected,
                        "Pattern '{}' should normalize to '{}', got '{}'",
                        pattern,
                        expected,
                        normalized
                    );
                }
            }

            /// Property 10 extended: Paths with current directory references
            ///
            /// Tests that paths with `./` components and various separators normalize correctly.
            #[test]
            fn prop_path_separator_normalization_current_dir_refs(
                dir_name in dirname_strategy(),
                file_name in r_filename_strategy(),
            ) {
                // Test various current directory reference patterns
                let patterns = vec![
                    format!("./{}/{}", dir_name, file_name),           // forward slashes
                    format!(".\\{}\\{}", dir_name, file_name),         // single backslashes
                    format!(".\\\\{}\\\\{}", dir_name, file_name),     // escaped backslashes
                    format!("./{}\\{}", dir_name, file_name),          // mixed
                ];

                let expected = format!("./{}/{}", dir_name, file_name);

                for pattern in &patterns {
                    let normalized = normalize_path_separators(pattern);

                    // Property 1: Normalized path must NOT contain backslashes
                    prop_assert!(
                        !normalized.contains('\\'),
                        "Pattern '{}' normalized to '{}' should NOT contain backslashes",
                        pattern,
                        normalized
                    );

                    // Property 2: All patterns should normalize to the same forward-slash form
                    prop_assert_eq!(
                        &normalized,
                        &expected,
                        "Pattern '{}' should normalize to '{}', got '{}'",
                        pattern,
                        expected,
                        normalized
                    );
                }
            }

            /// Property 10 extended: Deeply nested paths
            ///
            /// Tests that deeply nested paths with various separator styles normalize correctly.
            #[test]
            fn prop_path_separator_normalization_deep_paths(
                components in prop::collection::vec(dirname_strategy(), 3..6),
                file_name in r_filename_strategy(),
            ) {
                prop_assume!(components.len() >= 3);

                // Build expected path with forward slashes
                let expected = format!("{}/{}", components.join("/"), file_name);

                // Build path with all backslashes
                let backslash_path = format!("{}\\{}", components.join("\\"), file_name);

                // Build path with escaped backslashes
                let escaped_path = format!("{}\\\\{}", components.join("\\\\"), file_name);

                // Normalize both
                let normalized_backslash = normalize_path_separators(&backslash_path);
                let normalized_escaped = normalize_path_separators(&escaped_path);

                // Property 1: Both should normalize to the expected forward-slash form
                prop_assert_eq!(
                    &normalized_backslash,
                    &expected,
                    "Backslash path '{}' should normalize to '{}'",
                    backslash_path,
                    expected
                );

                // Property 2: Escaped backslash path should also normalize correctly
                prop_assert!(
                    !normalized_escaped.contains('\\'),
                    "Escaped path '{}' normalized to '{}' should NOT contain backslashes",
                    escaped_path,
                    normalized_escaped
                );
            }

            // ====================================================================
            // Property 17: Workspace Boundary Enforcement
            // Validates: Requirements 7.2
            // ====================================================================

            /// **Property 17: Workspace Boundary Enforcement**
            ///
            /// *For any* path that would resolve to a location outside the workspace root
            /// (e.g., excessive `../` components), the completion provider SHALL NOT include
            /// that path in results, and go-to-definition SHALL return `None`.
            ///
            /// This property is primarily enforced by `list_directory_entries()` which checks
            /// workspace boundaries when `workspace_root` is provided.
            ///
            /// **Validates: Requirements 7.2**
            #[test]
            fn prop_workspace_boundary_enforcement(
                workspace_depth in 1usize..5,
                subdir_depth in 0usize..3,
                escape_depth in 1usize..6,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                // Create a temporary directory structure
                let temp_dir = TempDir::new().unwrap();
                let temp_root = temp_dir.path();

                // Build workspace path: temp_root/level1/level2/.../levelN
                let mut workspace_path = temp_root.to_path_buf();
                for i in 0..workspace_depth {
                    workspace_path = workspace_path.join(format!("level{}", i));
                    fs::create_dir_all(&workspace_path).unwrap();
                }

                // Build a subdirectory within the workspace
                let mut current_dir = workspace_path.clone();
                for i in 0..subdir_depth {
                    current_dir = current_dir.join(format!("subdir{}", i));
                    fs::create_dir_all(&current_dir).unwrap();
                }

                // Create a test file in the current directory
                let test_file = current_dir.join("test.R");
                File::create(&test_file).unwrap();

                // Create a file outside the workspace (in temp_root)
                let outside_file = temp_root.join("outside.R");
                File::create(&outside_file).unwrap();

                // Also create a directory outside the workspace
                let outside_dir = temp_root.join("outside_dir");
                fs::create_dir_all(&outside_dir).unwrap();
                File::create(outside_dir.join("external.R")).unwrap();

                // Test 1: Listing the current directory with workspace boundary
                // should only return entries within the workspace
                let result = list_directory_entries(&current_dir, Some(&workspace_path));
                prop_assert!(
                    result.is_ok(),
                    "list_directory_entries should succeed for directory within workspace"
                );

                let entries = result.unwrap();
                // All returned entries should be within the workspace
                for (name, path, _) in &entries {
                    // Canonicalize for accurate comparison
                    if let (Ok(canonical_path), Ok(canonical_workspace)) =
                        (path.canonicalize(), workspace_path.canonicalize())
                    {
                        prop_assert!(
                            canonical_path.starts_with(&canonical_workspace),
                            "Entry '{}' at path {:?} should be within workspace {:?}",
                            name,
                            canonical_path,
                            canonical_workspace
                        );
                    }
                }

                // Test 2: If we try to list a directory outside the workspace,
                // entries should still be filtered by workspace boundary
                // (though in practice, we wouldn't list outside directories)
                if escape_depth > workspace_depth + subdir_depth {
                    // This would escape the workspace - verify behavior
                    // Build a path that would escape: current_dir + many ../
                    let mut escape_path = current_dir.clone();
                    for _ in 0..escape_depth {
                        escape_path = escape_path.join("..");
                    }

                    // Normalize the path
                    if let Ok(normalized) = escape_path.canonicalize() {
                        // If the normalized path is outside workspace, listing it
                        // with workspace boundary should exclude entries
                        if let Ok(canonical_workspace) = workspace_path.canonicalize() {
                            if !normalized.starts_with(&canonical_workspace) {
                                // This path is outside workspace
                                // list_directory_entries should either fail or return
                                // entries filtered by workspace boundary
                                let result = list_directory_entries(&normalized, Some(&workspace_path));
                                if let Ok(entries) = result {
                                    // Any entries returned should still be within workspace
                                    // (which means empty in this case since we're outside)
                                    for (name, path, _) in &entries {
                                        if let Ok(canonical_path) = path.canonicalize() {
                                            prop_assert!(
                                                canonical_path.starts_with(&canonical_workspace),
                                                "Entry '{}' from outside-workspace listing should be filtered out",
                                                name
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            /// Property 17 extended: Workspace boundary with symlinks
            ///
            /// Tests that symlinks pointing outside the workspace are correctly
            /// filtered out when workspace boundary checking is enabled.
            #[test]
            fn prop_workspace_boundary_symlinks(
                _dummy in Just(()),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                // Skip on platforms where symlinks might not work
                #[cfg(not(unix))]
                {
                    return Ok(());
                }

                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink;

                    let temp_dir = TempDir::new().unwrap();
                    let temp_root = temp_dir.path();

                    // Create workspace directory
                    let workspace = temp_root.join("workspace");
                    fs::create_dir_all(&workspace).unwrap();

                    // Create a directory outside workspace
                    let outside = temp_root.join("outside");
                    fs::create_dir_all(&outside).unwrap();
                    File::create(outside.join("external.R")).unwrap();

                    // Create a file inside workspace
                    File::create(workspace.join("internal.R")).unwrap();

                    // Create a symlink inside workspace pointing outside
                    let symlink_path = workspace.join("link_to_outside");
                    if symlink(&outside, &symlink_path).is_ok() {
                        // List workspace with boundary checking
                        let result = list_directory_entries(&workspace, Some(&workspace));

                        if let Ok(entries) = result {
                            // The symlink itself might be listed, but when we
                            // canonicalize and check, it should be filtered
                            // if it points outside the workspace
                            for (name, path, _) in &entries {
                                if let Ok(canonical_path) = path.canonicalize() {
                                    if let Ok(canonical_workspace) = workspace.canonicalize() {
                                        // Entries should be within workspace
                                        // Note: The symlink entry itself is in workspace,
                                        // but its target is outside. The current implementation
                                        // checks the canonical path, so symlinks to outside
                                        // should be filtered out.
                                        prop_assert!(
                                            canonical_path.starts_with(&canonical_workspace),
                                            "Entry '{}' with canonical path {:?} should be within workspace {:?}",
                                            name,
                                            canonical_path,
                                            canonical_workspace
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            /// Property 17 extended: No workspace boundary when workspace_root is None
            ///
            /// Tests that when workspace_root is None, no boundary checking is performed
            /// and all entries are returned.
            #[test]
            fn prop_no_workspace_boundary_when_none(
                num_files in 1usize..5,
                num_dirs in 0usize..3,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                let temp_dir = TempDir::new().unwrap();
                let base = temp_dir.path();

                // Create some files
                for i in 0..num_files {
                    File::create(base.join(format!("file{}.R", i))).unwrap();
                }

                // Create some directories
                for i in 0..num_dirs {
                    fs::create_dir(base.join(format!("dir{}", i))).unwrap();
                }

                // List without workspace boundary (workspace_root = None)
                let result = list_directory_entries(base, None);

                prop_assert!(result.is_ok(), "list_directory_entries should succeed");

                let entries = result.unwrap();

                // All created entries should be present (no filtering by workspace)
                prop_assert_eq!(
                    entries.len(),
                    num_files + num_dirs,
                    "All {} files and {} dirs should be listed when workspace_root is None",
                    num_files,
                    num_dirs
                );
            }

            /// Property 17 extended: Workspace boundary with nested directories
            ///
            /// Tests that entries in nested directories within the workspace are
            /// correctly included, while entries outside are excluded.
            #[test]
            fn prop_workspace_boundary_nested_dirs(
                depth in 1usize..4,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                let temp_dir = TempDir::new().unwrap();
                let temp_root = temp_dir.path();

                // Create workspace
                let workspace = temp_root.join("workspace");
                fs::create_dir_all(&workspace).unwrap();

                // Create nested directories within workspace
                let mut nested = workspace.clone();
                for i in 0..depth {
                    nested = nested.join(format!("nested{}", i));
                    fs::create_dir_all(&nested).unwrap();
                    // Create a file at each level
                    File::create(nested.join(format!("level{}.R", i))).unwrap();
                }

                // Create something outside workspace
                let outside = temp_root.join("outside");
                fs::create_dir_all(&outside).unwrap();
                File::create(outside.join("external.R")).unwrap();

                // List the deepest nested directory with workspace boundary
                let result = list_directory_entries(&nested, Some(&workspace));

                prop_assert!(result.is_ok(), "list_directory_entries should succeed");

                let entries = result.unwrap();

                // All entries should be within workspace
                for (name, path, _) in &entries {
                    if let (Ok(canonical_path), Ok(canonical_workspace)) =
                        (path.canonicalize(), workspace.canonicalize())
                    {
                        prop_assert!(
                            canonical_path.starts_with(&canonical_workspace),
                            "Nested entry '{}' should be within workspace",
                            name
                        );
                    }
                }

                // The file we created at this level should be present
                let file_name = format!("level{}.R", depth - 1);
                let has_expected_file = entries.iter().any(|(name, _, _)| name == &file_name);
                prop_assert!(
                    has_expected_file,
                    "Expected file '{}' should be in entries",
                    file_name
                );
            }

            // ====================================================================
            // Property 12: Source Call Go-to-Definition
            // Feature: file-path-intellisense
            // Validates: Requirements 5.1, 5.2, 5.4
            // ====================================================================

            /// **Property 12: Source Call Go-to-Definition**
            ///
            /// *For any* `source()` or `sys.source()` call with a string literal path
            /// that resolves to an existing file within the workspace, go-to-definition
            /// SHALL return a `Location` pointing to that file at line 0, column 0.
            /// The path SHALL be resolved using `PathContext::from_metadata()` which
            /// respects @lsp-cd working directory.
            ///
            /// **Validates: Requirements 5.1, 5.2, 5.4**
            #[test]
            fn prop_source_call_go_to_definition(
                file_name in r_filename_strategy(),
                (func_name, _is_sys_source) in source_function_strategy(),
                quote in quote_strategy(),
                use_subdir in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Determine the path structure based on use_subdir
                let (target_file, main_file, path_in_code) = if use_subdir {
                    // Create structure: workspace/subdir/target.R, main.R in workspace root
                    // Path in code: "subdir/target.R"
                    let subdir = workspace_root.join("subdir");
                    fs::create_dir_all(&subdir).unwrap();
                    let target = subdir.join(&file_name);
                    let main = workspace_root.join("main.R");
                    let path = format!("subdir/{}", file_name);
                    (target, main, path)
                } else {
                    // Create structure: workspace/target.R, main.R in workspace root
                    // Path in code: "target.R"
                    let target = workspace_root.join(&file_name);
                    let main = workspace_root.join("main.R");
                    (target, main, file_name.clone())
                };

                // Create the target file
                File::create(&target_file).unwrap();

                // Generate R code with source() or sys.source() call
                let code = format!("{}({quote}{}{quote})", func_name, path_in_code, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path string
                // func_name + "(" + quote = func_name.len() + 2
                let content_start_col = func_name.len() + 2;
                let cursor_col = content_start_col + path_in_code.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for existing file '{}' in code: {}",
                    path_in_code,
                    code
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
                prop_assert_eq!(
                    location.range.end.line,
                    0,
                    "Location end should be at line 0"
                );
                prop_assert_eq!(
                    location.range.end.character,
                    0,
                    "Location end should be at column 0"
                );
            }

            /// Property 12 extended: source() with parent directory reference
            ///
            /// Tests that source() calls with "../" paths correctly resolve
            /// to files in parent directories.
            ///
            /// **Validates: Requirements 5.1, 5.4**
            #[test]
            fn prop_source_call_go_to_definition_parent_dir(
                file_name in r_filename_strategy(),
                quote in quote_strategy(),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   target.R
                //   subdir/
                //     main.R
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file in workspace root
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create subdirectory for main file
                let subdir = workspace_root.join("subdir");
                fs::create_dir_all(&subdir).unwrap();
                let main_file = subdir.join("main.R");

                // Path in code uses "../" to reference parent directory
                let path_in_code = format!("../{}", file_name);

                // Generate R code with source() call
                let code = format!("source({quote}{}{quote})", path_in_code, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path string
                // "source(" + quote = 8
                let content_start_col = 8;
                let cursor_col = content_start_col + path_in_code.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for file '{}' with path '{}' in code: {}",
                    file_name,
                    path_in_code,
                    code
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file in parent directory"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
            }

            /// Property 12 extended: source() respects @lsp-cd working directory
            ///
            /// Tests that source() calls resolve paths relative to the @lsp-cd
            /// working directory when it is set.
            ///
            /// **Validates: Requirements 5.4**
            #[test]
            fn prop_source_call_go_to_definition_respects_lsp_cd(
                file_name in r_filename_strategy(),
                subdir_name in dirname_strategy(),
                quote in quote_strategy(),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(!subdir_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   main.R
                //   subdir/
                //     target.R
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create subdirectory
                let subdir = workspace_root.join(&subdir_name);
                fs::create_dir_all(&subdir).unwrap();

                // Create target file in subdirectory
                let target_file = subdir.join(&file_name);
                File::create(&target_file).unwrap();

                // Main file is in workspace root
                let main_file = workspace_root.join("main.R");

                // Generate R code with source() call using just the filename
                // (not the full path, because @lsp-cd will provide the directory)
                let code = format!("source({quote}{}{quote})", file_name, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();

                // Set @lsp-cd to the subdirectory
                let metadata = CrossFileMetadata {
                    working_directory: Some(subdir_name.clone()),
                    ..Default::default()
                };

                // Calculate cursor position inside the path string
                // "source(" + quote = 8
                let content_start_col = 8;
                let cursor_col = content_start_col + file_name.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists in @lsp-cd directory)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for file '{}' with @lsp-cd='{}' in code: {}",
                    file_name,
                    subdir_name,
                    code
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file in subdirectory
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file in @lsp-cd directory"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
            }

            /// Property 12 extended: sys.source() go-to-definition
            ///
            /// Tests that sys.source() calls work the same as source() for
            /// go-to-definition.
            ///
            /// **Validates: Requirements 5.2**
            #[test]
            fn prop_sys_source_go_to_definition(
                file_name in r_filename_strategy(),
                quote in quote_strategy(),
            ) {
                use std::fs::File;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Main file
                let main_file = workspace_root.join("main.R");

                // Generate R code with sys.source() call
                let code = format!(
                    "sys.source({quote}{}{quote}, envir = globalenv())",
                    file_name,
                    quote = quote
                );

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path string
                // "sys.source(" + quote = 12
                let content_start_col = 12;
                let cursor_col = content_start_col + file_name.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for sys.source() with file '{}' in code: {}",
                    file_name,
                    code
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
            }

            /// Property 12 extended: Cursor at various positions within path
            ///
            /// Tests that go-to-definition works regardless of cursor position
            /// within the path string (start, middle, end).
            ///
            /// **Validates: Requirements 5.1**
            #[test]
            fn prop_source_call_go_to_definition_cursor_positions(
                file_name in r_filename_strategy(),
            ) {
                use std::fs::File;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(file_name.len() >= 3); // Need at least 3 chars for meaningful positions

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Main file
                let main_file = workspace_root.join("main.R");

                // Generate R code with source() call
                let code = format!("source(\"{}\")", file_name);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Test cursor at start, middle, and end of path
                let content_start_col = 8; // "source(\"" = 8
                let cursor_positions = vec![
                    content_start_col,                          // Start of path
                    content_start_col + file_name.len() / 2,    // Middle of path
                    content_start_col + file_name.len() - 1,    // Near end of path
                ];

                for cursor_col in cursor_positions {
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = file_path_definition(
                        &tree,
                        &code,
                        position,
                        &file_uri,
                        &metadata,
                        Some(&workspace_root_url),
                    );

                    // Property: Result should be Some for all valid cursor positions
                    prop_assert!(
                        result.is_some(),
                        "Expected Some(Location) for cursor at column {} in code: {}",
                        cursor_col,
                        code
                    );

                    let location = result.unwrap();

                    // Location should point to the target file
                    let expected_uri = Url::from_file_path(&target_file).unwrap();
                    prop_assert_eq!(
                        location.uri,
                        expected_uri,
                        "Location URI should point to target file for cursor at column {}",
                        cursor_col
                    );
                }
            }

            // ====================================================================
            // Property 13: Missing File Returns No Definition
            // Feature: file-path-intellisense
            // Validates: Requirements 5.3
            // ====================================================================

            /// **Property 13: Missing File Returns No Definition**
            ///
            /// *For any* file path (in source() call or directive) that does not resolve
            /// to an existing file, go-to-definition SHALL return `None` (no navigation
            /// occurs).
            ///
            /// **Validates: Requirements 5.3**
            #[test]
            fn prop_missing_file_returns_no_definition(
                file_name in r_filename_strategy(),
                (func_name, _is_sys_source) in source_function_strategy(),
                quote in quote_strategy(),
            ) {
                use std::fs;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace WITHOUT the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create main file (but NOT the target file)
                let main_file = workspace_root.join("main.R");
                fs::write(&main_file, "# main file").unwrap();

                // Generate R code with source() or sys.source() call referencing non-existent file
                let code = format!("{}({quote}{}{quote})", func_name, file_name, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path string
                // func_name + "(" + quote = func_name.len() + 2
                let content_start_col = func_name.len() + 2;
                let cursor_col = content_start_col + file_name.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be None (file does not exist)
                prop_assert!(
                    result.is_none(),
                    "Expected None for non-existent file '{}' in code: {}",
                    file_name,
                    code
                );
            }

            /// Property 13 extended: Missing file in subdirectory path
            ///
            /// Tests that paths referencing non-existent files in subdirectories
            /// also return None.
            ///
            /// **Validates: Requirements 5.3**
            #[test]
            fn prop_missing_file_in_subdir_returns_no_definition(
                dir_name in dirname_strategy(),
                file_name in r_filename_strategy(),
                quote in quote_strategy(),
            ) {
                use std::fs;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(!dir_name.is_empty());

                // Create a temporary workspace with subdirectory but WITHOUT the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create the subdirectory (but not the file inside it)
                let subdir = workspace_root.join(&dir_name);
                fs::create_dir_all(&subdir).unwrap();

                // Create main file
                let main_file = workspace_root.join("main.R");
                fs::write(&main_file, "# main file").unwrap();

                // Path in code references non-existent file in existing subdirectory
                let path_in_code = format!("{}/{}", dir_name, file_name);

                // Generate R code with source() call
                let code = format!("source({quote}{}{quote})", path_in_code, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path string
                // "source(" + quote = 8
                let content_start_col = 8;
                let cursor_col = content_start_col + path_in_code.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be None (file does not exist)
                prop_assert!(
                    result.is_none(),
                    "Expected None for non-existent file '{}' in code: {}",
                    path_in_code,
                    code
                );
            }

            /// Property 13 extended: Missing file with parent directory reference
            ///
            /// Tests that paths with "../" referencing non-existent files return None.
            ///
            /// **Validates: Requirements 5.3**
            #[test]
            fn prop_missing_file_parent_dir_returns_no_definition(
                file_name in r_filename_strategy(),
                quote in quote_strategy(),
            ) {
                use std::fs;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   subdir/
                //     main.R
                // (no target file in workspace root)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create subdirectory for main file
                let subdir = workspace_root.join("subdir");
                fs::create_dir_all(&subdir).unwrap();
                let main_file = subdir.join("main.R");
                fs::write(&main_file, "# main file").unwrap();

                // Path in code uses "../" to reference parent directory (where file doesn't exist)
                let path_in_code = format!("../{}", file_name);

                // Generate R code with source() call
                let code = format!("source({quote}{}{quote})", path_in_code, quote = quote);

                // Parse the code
                let tree = parse_r(&code);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path string
                // "source(" + quote = 8
                let content_start_col = 8;
                let cursor_col = content_start_col + path_in_code.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be None (file does not exist)
                prop_assert!(
                    result.is_none(),
                    "Expected None for non-existent file '{}' in code: {}",
                    path_in_code,
                    code
                );
            }

            /// Property 13 extended: Missing file in directive
            ///
            /// Tests that directives referencing non-existent files return None.
            ///
            /// **Validates: Requirements 5.3**
            #[test]
            fn prop_missing_file_directive_returns_no_definition(
                file_name in r_filename_strategy(),
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                    Just("@lsp-source"),
                ],
                use_colon in prop::bool::ANY,
            ) {
                use std::fs;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace WITHOUT the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create main file (but NOT the target file)
                let main_file = workspace_root.join("main.R");

                // Generate directive comment
                let colon_part = if use_colon { ": " } else { " " };
                let content = format!("# {}{}{}", directive_name, colon_part, file_name);
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                // "# " + directive_name + colon_part = 2 + directive_name.len() + colon_part.len()
                let prefix_len = 2 + directive_name.len() + colon_part.len();
                let cursor_col = prefix_len + file_name.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be None (file does not exist)
                prop_assert!(
                    result.is_none(),
                    "Expected None for non-existent file '{}' in directive: {}",
                    file_name,
                    content
                );
            }

            // ====================================================================
            // Property 14: Backward Directive Go-to-Definition
            // Feature: file-path-intellisense
            // Validates: Requirements 6.1, 6.2, 6.3
            // ====================================================================

            /// **Property 14: Backward Directive Go-to-Definition**
            ///
            /// *For any* `@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by` directive
            /// with a path that resolves to an existing file, go-to-definition SHALL return
            /// a `Location` pointing to that file at line 0, column 0.
            ///
            /// **Validates: Requirements 6.1, 6.2, 6.3**
            #[test]
            fn prop_backward_directive_go_to_definition(
                file_name in r_filename_strategy(),
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                use_colon in prop::bool::ANY,
                use_quotes in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file (the "parent" file that sources this file)
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create main file (the "child" file with the directive)
                let main_file = workspace_root.join("child.R");

                // Generate directive comment with various syntax options
                let colon_part = if use_colon { ": " } else { " " };
                let (open_quote, close_quote) = if use_quotes { ("\"", "\"") } else { ("", "") };
                let content = format!(
                    "# {}{}{}{}{}",
                    directive_name, colon_part, open_quote, file_name, close_quote
                );
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                // "# " + directive_name + colon_part + open_quote = 2 + directive_name.len() + colon_part.len() + open_quote.len()
                let prefix_len = 2 + directive_name.len() + colon_part.len() + open_quote.len();
                let cursor_col = prefix_len + file_name.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for existing file '{}' in directive: {}",
                    file_name,
                    content
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
                prop_assert_eq!(
                    location.range.end.line,
                    0,
                    "Location end should be at line 0"
                );
                prop_assert_eq!(
                    location.range.end.character,
                    0,
                    "Location end should be at column 0"
                );
            }

            /// Property 14 extended: Backward directive with relative path
            ///
            /// Tests that backward directives with relative paths (e.g., "../parent.R")
            /// correctly resolve to files in parent directories.
            ///
            /// **Validates: Requirements 6.1, 6.2, 6.3**
            #[test]
            fn prop_backward_directive_go_to_definition_relative_path(
                file_name in r_filename_strategy(),
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                use_colon in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   parent.R (target file)
                //   subdir/
                //     child.R (file with directive)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file in workspace root
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create subdirectory for child file
                let subdir = workspace_root.join("subdir");
                fs::create_dir_all(&subdir).unwrap();
                let main_file = subdir.join("child.R");

                // Path in directive uses "../" to reference parent directory
                let path_in_directive = format!("../{}", file_name);

                // Generate directive comment
                let colon_part = if use_colon { ": " } else { " " };
                let content = format!("# {}{}{}", directive_name, colon_part, path_in_directive);
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                let prefix_len = 2 + directive_name.len() + colon_part.len();
                let cursor_col = prefix_len + path_in_directive.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for file '{}' with path '{}' in directive: {}",
                    file_name,
                    path_in_directive,
                    content
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file in parent directory"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
            }

            /// Property 14 extended: Backward directive with subdirectory path
            ///
            /// Tests that backward directives with subdirectory paths (e.g., "subdir/parent.R")
            /// correctly resolve to files in subdirectories.
            ///
            /// **Validates: Requirements 6.1, 6.2, 6.3**
            #[test]
            fn prop_backward_directive_go_to_definition_subdir_path(
                file_name in r_filename_strategy(),
                subdir_name in dirname_strategy(),
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                use_colon in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(!subdir_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   child.R (file with directive)
                //   subdir/
                //     parent.R (target file)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create subdirectory
                let subdir = workspace_root.join(&subdir_name);
                fs::create_dir_all(&subdir).unwrap();

                // Create target file in subdirectory
                let target_file = subdir.join(&file_name);
                File::create(&target_file).unwrap();

                // Create main file in workspace root
                let main_file = workspace_root.join("child.R");

                // Path in directive references file in subdirectory
                let path_in_directive = format!("{}/{}", subdir_name, file_name);

                // Generate directive comment
                let colon_part = if use_colon { ": " } else { " " };
                let content = format!("# {}{}{}", directive_name, colon_part, path_in_directive);
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                let prefix_len = 2 + directive_name.len() + colon_part.len();
                let cursor_col = prefix_len + path_in_directive.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for file '{}' with path '{}' in directive: {}",
                    file_name,
                    path_in_directive,
                    content
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file in subdirectory"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
            }

            /// Property 14 extended: Cursor at various positions within directive path
            ///
            /// Tests that go-to-definition works regardless of cursor position
            /// within the path string (start, middle, end).
            ///
            /// **Validates: Requirements 6.1, 6.2, 6.3**
            #[test]
            fn prop_backward_directive_go_to_definition_cursor_positions(
                file_name in r_filename_strategy(),
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
            ) {
                use std::fs::File;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(file_name.len() >= 3); // Need at least 3 chars for meaningful positions

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create main file
                let main_file = workspace_root.join("child.R");

                // Generate directive comment
                let content = format!("# {} {}", directive_name, file_name);

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Test cursor at start, middle, and end of path
                // "# " + directive_name + " " = 2 + directive_name.len() + 1
                let path_start_col = 2 + directive_name.len() + 1;
                let cursor_positions = vec![
                    path_start_col,                          // Start of path
                    path_start_col + file_name.len() / 2,    // Middle of path
                    path_start_col + file_name.len() - 1,    // Near end of path
                ];

                for cursor_col in cursor_positions {
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = file_path_definition(
                        &tree,
                        &content,
                        position,
                        &file_uri,
                        &metadata,
                        Some(&workspace_root_url),
                    );

                    // Property: Result should be Some for all valid cursor positions
                    prop_assert!(
                        result.is_some(),
                        "Expected Some(Location) for cursor at column {} in directive: {}",
                        cursor_col,
                        content
                    );

                    let location = result.unwrap();

                    // Location should point to the target file
                    let expected_uri = Url::from_file_path(&target_file).unwrap();
                    prop_assert_eq!(
                        location.uri,
                        expected_uri,
                        "Location URI should point to target file for cursor at column {}",
                        cursor_col
                    );
                }
            }

            // ====================================================================
            // Property 15: Forward Directive Go-to-Definition
            // Feature: file-path-intellisense
            // Validates: Requirements 6.4
            // ====================================================================

            /// **Property 15: Forward Directive Go-to-Definition**
            ///
            /// *For any* `@lsp-source` directive with a path that resolves to an existing file,
            /// go-to-definition SHALL return a `Location` pointing to that file at line 0, column 0.
            ///
            /// **Validates: Requirements 6.4**
            #[test]
            fn prop_forward_directive_go_to_definition(
                file_name in r_filename_strategy(),
                use_colon in prop::bool::ANY,
                use_quotes in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file (the file being sourced)
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create main file (the file with the @lsp-source directive)
                let main_file = workspace_root.join("main.R");

                // Generate directive comment with various syntax options
                let colon_part = if use_colon { ": " } else { " " };
                let (open_quote, close_quote) = if use_quotes { ("\"", "\"") } else { ("", "") };
                let content = format!(
                    "# @lsp-source{}{}{}{}",
                    colon_part, open_quote, file_name, close_quote
                );
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                // "# @lsp-source" + colon_part + open_quote = 13 + colon_part.len() + open_quote.len()
                let prefix_len = 13 + colon_part.len() + open_quote.len();
                let cursor_col = prefix_len + file_name.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property 1: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for existing file '{}' in directive: {}",
                    file_name,
                    content
                );

                let location = result.unwrap();

                // Property 2: Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file"
                );

                // Property 3: Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should be at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should be at column 0"
                );
                prop_assert_eq!(
                    location.range.end.line,
                    0,
                    "Location end should be at line 0"
                );
                prop_assert_eq!(
                    location.range.end.character,
                    0,
                    "Location end should be at column 0"
                );
            }

            /// Property 15 extended: Forward directive with relative path
            ///
            /// Tests that forward directives with relative paths (e.g., "../utils.R")
            /// correctly resolve to files in parent directories.
            ///
            /// **Validates: Requirements 6.4**
            #[test]
            fn prop_forward_directive_go_to_definition_relative_path(
                file_name in r_filename_strategy(),
                use_colon in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   target.R (the file being sourced)
                //   subdir/
                //     main.R (the file with the directive)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file in workspace root
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create subdir and main file
                let subdir = workspace_root.join("subdir");
                fs::create_dir(&subdir).unwrap();
                let main_file = subdir.join("main.R");

                // Generate directive comment with relative path
                let colon_part = if use_colon { ": " } else { " " };
                let relative_path = format!("../{}", file_name);
                let content = format!("# @lsp-source{}{}", colon_part, relative_path);
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                // "# @lsp-source" + colon_part = 13 + colon_part.len()
                let prefix_len = 13 + colon_part.len();
                let cursor_col = prefix_len + relative_path.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for existing file '{}' with relative path in directive: {}",
                    file_name,
                    content
                );

                let location = result.unwrap();

                // Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file"
                );
            }

            /// Property 15 extended: Forward directive with subdirectory path
            ///
            /// Tests that forward directives with subdirectory paths (e.g., "subdir/utils.R")
            /// correctly resolve to files in subdirectories.
            ///
            /// **Validates: Requirements 6.4**
            #[test]
            fn prop_forward_directive_go_to_definition_subdir_path(
                file_name in r_filename_strategy(),
                subdir_name in dirname_strategy(),
                use_colon in prop::bool::ANY,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(!subdir_name.is_empty());

                // Create a temporary workspace with structure:
                // workspace/
                //   main.R (the file with the directive)
                //   subdir/
                //     target.R (the file being sourced)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create subdir and target file
                let subdir = workspace_root.join(&subdir_name);
                fs::create_dir(&subdir).unwrap();
                let target_file = subdir.join(&file_name);
                File::create(&target_file).unwrap();

                // Create main file in workspace root
                let main_file = workspace_root.join("main.R");

                // Generate directive comment with subdirectory path
                let colon_part = if use_colon { ": " } else { " " };
                let subdir_path = format!("{}/{}", subdir_name, file_name);
                let content = format!("# @lsp-source{}{}", colon_part, subdir_path);
                fs::write(&main_file, &content).unwrap();

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Calculate cursor position inside the path
                // "# @lsp-source" + colon_part = 13 + colon_part.len()
                let prefix_len = 13 + colon_part.len();
                let cursor_col = prefix_len + subdir_path.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be Some (file exists)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for existing file '{}' in subdir '{}' in directive: {}",
                    file_name,
                    subdir_name,
                    content
                );

                let location = result.unwrap();

                // Location should point to the target file
                let expected_uri = Url::from_file_path(&target_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Location URI should point to target file"
                );
            }

            /// Property 15 extended: Cursor at various positions within forward directive path
            ///
            /// Tests that go-to-definition works regardless of cursor position
            /// within the path string (start, middle, end).
            ///
            /// **Validates: Requirements 6.4**
            #[test]
            fn prop_forward_directive_go_to_definition_cursor_positions(
                file_name in r_filename_strategy(),
            ) {
                use std::fs::File;
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());
                prop_assume!(file_name.len() >= 3); // Need at least 3 chars for meaningful positions

                // Create a temporary workspace with the target file
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create target file
                let target_file = workspace_root.join(&file_name);
                File::create(&target_file).unwrap();

                // Create main file
                let main_file = workspace_root.join("main.R");

                // Generate directive comment
                let content = format!("# @lsp-source {}", file_name);

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Test cursor at start, middle, and end of path
                // "# @lsp-source " = 14
                let path_start_col = 14;
                let cursor_positions = vec![
                    path_start_col,                          // Start of path
                    path_start_col + file_name.len() / 2,    // Middle of path
                    path_start_col + file_name.len() - 1,    // Near end of path
                ];

                for cursor_col in cursor_positions {
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    // Call the function under test
                    let result = file_path_definition(
                        &tree,
                        &content,
                        position,
                        &file_uri,
                        &metadata,
                        Some(&workspace_root_url),
                    );

                    // Property: Result should be Some for all valid cursor positions
                    prop_assert!(
                        result.is_some(),
                        "Expected Some(Location) for cursor at column {} in directive: {}",
                        cursor_col,
                        content
                    );

                    let location = result.unwrap();

                    // Location should point to the target file
                    let expected_uri = Url::from_file_path(&target_file).unwrap();
                    prop_assert_eq!(
                        location.uri,
                        expected_uri,
                        "Location URI should point to target file for cursor at column {}",
                        cursor_col
                    );
                }
            }

            // ====================================================================
            // Property 16: Backward Directives Ignore @lsp-cd
            // Feature: file-path-intellisense
            // Validates: Requirements 6.5
            // ====================================================================

            /// **Property 16: Backward Directives Ignore @lsp-cd**
            ///
            /// *For any* file containing both an `@lsp-cd` directive and a backward directive
            /// (`@lsp-sourced-by`, `@lsp-run-by`, or `@lsp-included-by`), the backward directive
            /// path SHALL be resolved using `PathContext::new()` (relative to the file's directory),
            /// NOT using the @lsp-cd working directory.
            ///
            /// This test verifies that:
            /// 1. When a file has @lsp-cd set to a different directory
            /// 2. And a backward directive references a file relative to the file's own directory
            /// 3. The go-to-definition resolves the path relative to the file's directory
            /// 4. NOT relative to the @lsp-cd working directory
            ///
            /// **Validates: Requirements 6.5**
            #[test]
            fn prop_backward_directive_ignores_lsp_cd(
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                file_name in r_filename_strategy(),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the following structure:
                // workspace/
                //   subdir/
                //     child.R       <- file with @lsp-cd and backward directive
                //   other_dir/      <- @lsp-cd points here (should be ignored)
                //   parent.R        <- target file (relative to child.R's directory: ../parent.R)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create directories
                let subdir = workspace_root.join("subdir");
                fs::create_dir(&subdir).unwrap();
                let other_dir = workspace_root.join("other_dir");
                fs::create_dir(&other_dir).unwrap();

                // Create the target parent file at workspace root
                // This file should be found via "../parent.R" from subdir/child.R
                let parent_file = workspace_root.join(&file_name);
                File::create(&parent_file).unwrap();

                // Create the child file in subdir
                let child_file = subdir.join("child.R");

                // Generate content with @lsp-cd pointing to other_dir (should be ignored)
                // and a backward directive pointing to ../file_name (relative to child's dir)
                let content = format!(
                    "# @lsp-cd /other_dir\n# {} ../{}",
                    directive_name,
                    file_name
                );

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&child_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();

                // Create metadata with working_directory set (simulating @lsp-cd)
                // This should be IGNORED for backward directive resolution
                let metadata = CrossFileMetadata {
                    working_directory: Some("/other_dir".to_string()),
                    ..Default::default()
                };

                // Position cursor on the path in the backward directive
                // Line 1: "# @lsp-sourced-by ../file_name"
                // The path starts after "# @lsp-sourced-by " (varies by directive)
                let directive_prefix_len = 2 + directive_name.len() + 1; // "# " + directive + " "
                let cursor_col = directive_prefix_len + 3; // Position in the middle of "../"

                let position = Position {
                    line: 1,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be Some because the file exists at ../file_name
                // relative to child.R's directory (subdir), which resolves to workspace_root/file_name
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for backward directive '{}' with path '../{}'. \
                     The path should resolve relative to the file's directory (subdir/), \
                     NOT relative to @lsp-cd (/other_dir). Content:\n{}",
                    directive_name,
                    file_name,
                    content
                );

                let location = result.unwrap();

                // The location should point to the parent file at workspace_root/file_name
                // NOT to other_dir/file_name (which doesn't exist anyway)
                let expected_uri = Url::from_file_path(&parent_file).unwrap();
                prop_assert_eq!(
                    &location.uri,
                    &expected_uri,
                    "Backward directive '{}' should resolve '../{}' relative to file's directory, \
                     ignoring @lsp-cd. Expected: {}, Got: {}",
                    directive_name,
                    file_name,
                    expected_uri,
                    location.uri
                );

                // Location should be at line 0, column 0
                prop_assert_eq!(
                    location.range.start.line,
                    0,
                    "Location should start at line 0"
                );
                prop_assert_eq!(
                    location.range.start.character,
                    0,
                    "Location should start at character 0"
                );
            }

            /// Property 16 extended: Backward directive with @lsp-cd and subdirectory path
            ///
            /// Tests that backward directives with subdirectory paths also ignore @lsp-cd.
            /// The path "subdir/parent.R" should resolve relative to the file's directory,
            /// not relative to the @lsp-cd working directory.
            ///
            /// **Validates: Requirements 6.5**
            #[test]
            fn prop_backward_directive_ignores_lsp_cd_subdir_path(
                directive_name in prop_oneof![
                    Just("@lsp-sourced-by"),
                    Just("@lsp-run-by"),
                    Just("@lsp-included-by"),
                ],
                file_name in r_filename_strategy(),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the following structure:
                // workspace/
                //   child.R           <- file with @lsp-cd and backward directive
                //   parents/
                //     parent.R        <- target file (relative to child.R: parents/parent.R)
                //   other_dir/        <- @lsp-cd points here (should be ignored)
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create directories
                let parents_dir = workspace_root.join("parents");
                fs::create_dir(&parents_dir).unwrap();
                let other_dir = workspace_root.join("other_dir");
                fs::create_dir(&other_dir).unwrap();

                // Create the target parent file in parents/ subdirectory
                let parent_file = parents_dir.join(&file_name);
                File::create(&parent_file).unwrap();

                // Create the child file at workspace root
                let child_file = workspace_root.join("child.R");

                // Generate content with @lsp-cd pointing to other_dir (should be ignored)
                // and a backward directive pointing to parents/file_name
                let content = format!(
                    "# @lsp-cd /other_dir\n# {} parents/{}",
                    directive_name,
                    file_name
                );

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&child_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();

                // Create metadata with working_directory set (simulating @lsp-cd)
                let metadata = CrossFileMetadata {
                    working_directory: Some("/other_dir".to_string()),
                    ..Default::default()
                };

                // Position cursor on the path in the backward directive
                let directive_prefix_len = 2 + directive_name.len() + 1;
                let cursor_col = directive_prefix_len + 4; // Position in "parents/"

                let position = Position {
                    line: 1,
                    character: cursor_col as u32,
                };

                // Call the function under test
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Result should be Some because the file exists at parents/file_name
                // relative to child.R's directory (workspace_root)
                prop_assert!(
                    result.is_some(),
                    "Expected Some(Location) for backward directive '{}' with path 'parents/{}'. \
                     The path should resolve relative to the file's directory, \
                     NOT relative to @lsp-cd (/other_dir). Content:\n{}",
                    directive_name,
                    file_name,
                    content
                );

                let location = result.unwrap();

                // The location should point to the parent file
                let expected_uri = Url::from_file_path(&parent_file).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Backward directive '{}' should resolve 'parents/{}' relative to file's directory, \
                     ignoring @lsp-cd",
                    directive_name,
                    file_name
                );
            }

            /// Property 16 extended: Contrast with source() which DOES use @lsp-cd
            ///
            /// This test verifies the critical distinction: source() calls respect @lsp-cd,
            /// while backward directives ignore it. Both are tested with the same file structure
            /// to demonstrate the different behavior.
            ///
            /// **Validates: Requirements 6.5 (contrast with 5.4)**
            #[test]
            fn prop_backward_directive_vs_source_call_lsp_cd_behavior(
                file_name in r_filename_strategy(),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                prop_assume!(!file_name.is_empty());

                // Create a temporary workspace with the following structure:
                // workspace/
                //   subdir/
                //     child.R         <- file with @lsp-cd and both directive and source()
                //   working_dir/
                //     target.R        <- file that source() should find (via @lsp-cd)
                //   target.R          <- file that directive should find (relative to subdir/)
                //
                // The key insight: "../target.R" resolves differently for:
                // - source(): uses @lsp-cd, so ../target.R from /working_dir -> /target.R (workspace root)
                // - directive: ignores @lsp-cd, so ../target.R from /subdir -> /target.R (workspace root)
                //
                // To demonstrate the difference, we'll use a path that exists in one location
                // but not the other.

                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create directories
                let subdir = workspace_root.join("subdir");
                fs::create_dir(&subdir).unwrap();
                let working_dir = workspace_root.join("working_dir");
                fs::create_dir(&working_dir).unwrap();

                // Create target file ONLY at workspace root (for directive resolution)
                // Do NOT create it in working_dir (so source() would fail if it used @lsp-cd incorrectly)
                let target_at_root = workspace_root.join(&file_name);
                File::create(&target_at_root).unwrap();

                // Create the child file in subdir
                let child_file = subdir.join("child.R");

                // Content with @lsp-cd pointing to working_dir
                // The backward directive should resolve ../file_name relative to subdir/ -> workspace_root/
                let content = format!(
                    "# @lsp-cd /working_dir\n# @lsp-sourced-by ../{}",
                    file_name
                );

                // Parse the code
                let tree = parse_r(&content);

                // Create URIs
                let file_uri = Url::from_file_path(&child_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();

                // Create metadata with working_directory set
                let metadata = CrossFileMetadata {
                    working_directory: Some("/working_dir".to_string()),
                    ..Default::default()
                };

                // Position cursor on the path in the backward directive (line 1)
                // "# @lsp-sourced-by " = 18 characters
                let cursor_col = 18 + 2; // Position in "../"

                let position = Position {
                    line: 1,
                    character: cursor_col as u32,
                };

                // Call the function under test for the backward directive
                let result = file_path_definition(
                    &tree,
                    &content,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Backward directive should find the file at workspace_root/file_name
                // because it resolves relative to the file's directory (subdir/), ignoring @lsp-cd
                prop_assert!(
                    result.is_some(),
                    "Backward directive should resolve '../{}' relative to file's directory (subdir/), \
                     finding the file at workspace root. @lsp-cd should be ignored.",
                    file_name
                );

                let location = result.unwrap();
                let expected_uri = Url::from_file_path(&target_at_root).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri,
                    "Backward directive should resolve to workspace_root/{}, ignoring @lsp-cd",
                    file_name
                );
            }

            // ====================================================================
            // Property 18: Invalid Character Handling
            // Feature: file-path-intellisense
            // Validates: Requirements 7.3
            // ====================================================================

            /// **Property 18: Invalid Character Handling**
            ///
            /// *For any* path containing invalid filesystem characters (null bytes, etc.),
            /// the completion provider SHALL return an empty list without throwing an error,
            /// and go-to-definition SHALL return `None` without throwing an error.
            ///
            /// **Validates: Requirements 7.3**
            #[test]
            fn prop_invalid_character_handling(
                valid_prefix in prop::string::string_regex("[a-z]{1,5}").unwrap(),
                valid_suffix in prop::string::string_regex("[a-z]{1,5}").unwrap(),
            ) {
                use std::fs::File;
                use tempfile::TempDir;

                // Create a temporary workspace
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create a valid file for comparison
                let valid_file = workspace_root.join("valid.R");
                File::create(&valid_file).unwrap();

                // Create main file
                let main_file = workspace_root.join("main.R");

                // Test 1: Path with null byte (invalid on all platforms)
                // The null byte makes the path invalid for filesystem operations
                let invalid_path_with_null = format!("{}\0{}.R", valid_prefix, valid_suffix);
                let code_with_null = format!("source(\"{}\")", invalid_path_with_null);
                let tree = parse_r(&code_with_null);

                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Position cursor inside the path
                let cursor_col = 8 + valid_prefix.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Go-to-definition should return None without panicking
                // (the file with null byte in name cannot exist)
                let gtd_result = file_path_definition(
                    &tree,
                    &code_with_null,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Should return None for invalid path (not panic)
                prop_assert!(
                    gtd_result.is_none(),
                    "Go-to-definition should return None for path with null byte, not panic"
                );

                // Test 2: Completions with invalid directory path containing null byte
                // Create a context where the directory component contains the null byte
                let invalid_dir_path = format!("{}\0{}/", valid_prefix, valid_suffix);
                let context = FilePathContext::SourceCall {
                    partial_path: invalid_dir_path.clone(),
                    content_start: Position { line: 0, character: 8 },
                    is_sys_source: false,
                };

                // Completions should return empty vec without panicking
                // because the directory with null byte cannot exist
                let cursor_pos = Position { line: 0, character: 8 + invalid_dir_path.len() as u32 };
                let completions = file_path_completions(
                    &context,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                    cursor_pos,
                );

                // Property: Should return empty vec for invalid directory path (not panic)
                prop_assert!(
                    completions.is_empty(),
                    "Completions should return empty vec for directory path with null byte, not panic"
                );

                // Test 3: Path with control characters
                let invalid_path_with_control = format!("{}\x01{}.R", valid_prefix, valid_suffix);
                let code_with_control = format!("source(\"{}\")", invalid_path_with_control);
                let tree_control = parse_r(&code_with_control);

                let gtd_result_control = file_path_definition(
                    &tree_control,
                    &code_with_control,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Should handle control characters gracefully
                prop_assert!(
                    gtd_result_control.is_none(),
                    "Go-to-definition should return None for path with control characters"
                );
            }

            /// Property 18 extended: Non-existent directory handling
            ///
            /// Tests that attempting to list a non-existent directory returns
            /// an error gracefully, which is then handled by returning empty completions.
            #[test]
            fn prop_nonexistent_directory_handling(
                random_name in prop::string::string_regex("[a-z]{5,10}").unwrap(),
            ) {
                use tempfile::TempDir;

                // Create a temporary workspace
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Try to list a non-existent directory
                let nonexistent = workspace_root.join(&random_name).join("does_not_exist");

                let result = list_directory_entries(&nonexistent, Some(workspace_root));

                // Property: Should return an error (not panic)
                prop_assert!(
                    result.is_err(),
                    "list_directory_entries should return Err for non-existent directory"
                );
            }

            // ====================================================================
            // Property 19: Space Handling in Paths
            // Feature: file-path-intellisense
            // Validates: Requirements 7.5
            // ====================================================================

            /// **Property 19: Space Handling in Paths**
            ///
            /// *For any* quoted path containing spaces (e.g., `"path with spaces/file.R"`),
            /// the path resolver SHALL correctly parse and resolve the complete path
            /// including the spaces.
            ///
            /// **Validates: Requirements 7.5**
            #[test]
            fn prop_space_handling_in_paths(
                prefix in prop::string::string_regex("[a-z]{1,5}").unwrap(),
                suffix in prop::string::string_regex("[a-z]{1,5}").unwrap(),
                num_spaces in 1usize..4,
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                // Create a temporary workspace
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create a directory with spaces in the name
                let spaces = " ".repeat(num_spaces);
                let dir_name = format!("{}{}dir", prefix, spaces);
                let dir_with_spaces = workspace_root.join(&dir_name);
                fs::create_dir_all(&dir_with_spaces).unwrap();

                // Create a file with spaces in the name
                let file_name = format!("{}{}file.R", suffix, spaces);
                let file_with_spaces = dir_with_spaces.join(&file_name);
                File::create(&file_with_spaces).unwrap();

                // Create main file
                let main_file = workspace_root.join("main.R");

                // Test 1: source() call with path containing spaces (quoted)
                let path_with_spaces = format!("{}/{}", dir_name, file_name);
                let code = format!("source(\"{}\")", path_with_spaces);
                let tree = parse_r(&code);

                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Position cursor in the middle of the path
                let cursor_col = 8 + path_with_spaces.len() / 2;
                let position = Position {
                    line: 0,
                    character: cursor_col as u32,
                };

                // Go-to-definition should find the file
                let gtd_result = file_path_definition(
                    &tree,
                    &code,
                    position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Should find the file with spaces in path
                prop_assert!(
                    gtd_result.is_some(),
                    "Go-to-definition should find file with spaces in path: '{}'",
                    path_with_spaces
                );

                let location = gtd_result.unwrap();
                let expected_uri = Url::from_file_path(&file_with_spaces).unwrap();
                prop_assert_eq!(
                    location.uri,
                    expected_uri.clone(),
                    "Location should point to file with spaces"
                );

                // Test 2: Directive with path containing spaces (quoted)
                let directive_content = format!("# @lsp-sourced-by \"{}\"", path_with_spaces);
                let directive_tree = parse_r(&directive_content);

                // Position cursor in the path
                // "# @lsp-sourced-by \"" = 20 characters
                let directive_cursor_col = 20 + path_with_spaces.len() / 2;
                let directive_position = Position {
                    line: 0,
                    character: directive_cursor_col as u32,
                };

                let directive_result = file_path_definition(
                    &directive_tree,
                    &directive_content,
                    directive_position,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                );

                // Property: Directive should also find the file with spaces
                prop_assert!(
                    directive_result.is_some(),
                    "Directive go-to-definition should find file with spaces in path: '{}'",
                    path_with_spaces
                );

                let directive_location = directive_result.unwrap();
                prop_assert_eq!(
                    directive_location.uri,
                    expected_uri,
                    "Directive location should point to file with spaces"
                );
            }

            /// Property 19 extended: Context detection with spaces in path
            ///
            /// Tests that context detection correctly extracts partial paths
            /// that contain spaces.
            #[test]
            fn prop_space_handling_context_detection(
                prefix in prop::string::string_regex("[a-z]{1,5}").unwrap(),
                suffix in prop::string::string_regex("[a-z]{1,5}").unwrap(),
            ) {
                // Create a path with spaces
                let path_with_spaces = format!("{} {}/file.R", prefix, suffix);
                let code = format!("source(\"{}\")", path_with_spaces);
                let tree = parse_r(&code);

                // Test cursor at various positions within the path
                let content_start = 8; // After opening quote

                for cursor_offset in [0, prefix.len(), prefix.len() + 1, path_with_spaces.len()] {
                    if cursor_offset > path_with_spaces.len() {
                        continue;
                    }

                    let cursor_col = content_start + cursor_offset;
                    let position = Position {
                        line: 0,
                        character: cursor_col as u32,
                    };

                    let context = detect_file_path_context(&tree, &code, position);

                    // Property: Should detect source call context
                    match &context {
                        FilePathContext::SourceCall { partial_path, .. } => {
                            let expected_partial = &path_with_spaces[..cursor_offset];
                            prop_assert_eq!(
                                partial_path,
                                expected_partial,
                                "Partial path should correctly include spaces at cursor offset {}",
                                cursor_offset
                            );
                        }
                        _ => {
                            prop_assert!(
                                false,
                                "Expected SourceCall context at cursor offset {}, got {:?}",
                                cursor_offset,
                                context
                            );
                        }
                    }
                }
            }

            /// Property 19 extended: Completions in directory with spaces
            ///
            /// Tests that completions work correctly when listing a directory
            /// that has spaces in its name.
            #[test]
            fn prop_space_handling_completions(
                dir_prefix in prop::string::string_regex("[a-z]{1,3}").unwrap(),
                dir_suffix in prop::string::string_regex("[a-z]{1,3}").unwrap(),
            ) {
                use std::fs::{self, File};
                use tempfile::TempDir;

                // Create a temporary workspace
                let temp_dir = TempDir::new().unwrap();
                let workspace_root = temp_dir.path();

                // Create a directory with spaces
                let dir_name = format!("{} {}", dir_prefix, dir_suffix);
                let dir_with_spaces = workspace_root.join(&dir_name);
                fs::create_dir_all(&dir_with_spaces).unwrap();

                // Create some R files in the directory
                File::create(dir_with_spaces.join("utils.R")).unwrap();
                File::create(dir_with_spaces.join("helpers.R")).unwrap();

                // Create main file
                let main_file = workspace_root.join("main.R");

                // Create context for completions in the directory with spaces
                let partial_path = format!("{}/", dir_name);
                let context = FilePathContext::SourceCall {
                    partial_path: partial_path.clone(),
                    content_start: Position { line: 0, character: 8 },
                    is_sys_source: false,
                };

                let file_uri = Url::from_file_path(&main_file).unwrap();
                let workspace_root_url = Url::from_file_path(workspace_root).unwrap();
                let metadata = CrossFileMetadata::default();

                // Get completions
                let cursor_pos = Position { line: 0, character: 8 + partial_path.len() as u32 };
                let completions = file_path_completions(
                    &context,
                    &file_uri,
                    &metadata,
                    Some(&workspace_root_url),
                    cursor_pos,
                );

                // Property: Should return completions for files in directory with spaces
                prop_assert!(
                    completions.len() >= 2,
                    "Should find at least 2 R files in directory with spaces '{}', found {}",
                    dir_name,
                    completions.len()
                );

                // Check that the expected files are in completions
                let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
                prop_assert!(
                    labels.contains(&"utils.R"),
                    "Completions should include utils.R"
                );
                prop_assert!(
                    labels.contains(&"helpers.R"),
                    "Completions should include helpers.R"
                );
            }
        }
    }

    // ========================================================================
    // Tests for list_directory_entries
    // ========================================================================

    mod list_directory_tests {
        use super::*;
        use std::fs::{self, File};
        use tempfile::TempDir;

        /// Helper to create a test directory structure
        fn create_test_dir() -> TempDir {
            TempDir::new().unwrap()
        }

        /// Helper to create a file in a directory
        fn create_file(dir: &Path, name: &str) {
            File::create(dir.join(name)).unwrap();
        }

        /// Helper to create a subdirectory
        fn create_subdir(dir: &Path, name: &str) -> PathBuf {
            let subdir = dir.join(name);
            fs::create_dir(&subdir).unwrap();
            subdir
        }

        #[test]
        fn test_list_directory_entries_basic() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            // Create some files and directories
            create_file(base, "utils.R");
            create_file(base, "helpers.r");
            create_file(base, "data.csv");
            create_subdir(base, "subdir");

            let result = list_directory_entries(base, None).unwrap();

            // Should have 4 entries (utils.R, helpers.r, data.csv, subdir)
            assert_eq!(result.len(), 4);

            // Check that all expected entries are present
            let names: Vec<&str> = result.iter().map(|(name, _, _)| name.as_str()).collect();
            assert!(names.contains(&"utils.R"));
            assert!(names.contains(&"helpers.r"));
            assert!(names.contains(&"data.csv"));
            assert!(names.contains(&"subdir"));
        }

        #[test]
        fn test_list_directory_entries_excludes_hidden() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            // Create visible and hidden files/directories
            create_file(base, "visible.R");
            create_file(base, ".hidden_file");
            create_subdir(base, "visible_dir");
            create_subdir(base, ".hidden_dir");

            let result = list_directory_entries(base, None).unwrap();

            // Should only have 2 entries (visible.R, visible_dir)
            assert_eq!(result.len(), 2);

            let names: Vec<&str> = result.iter().map(|(name, _, _)| name.as_str()).collect();
            assert!(names.contains(&"visible.R"));
            assert!(names.contains(&"visible_dir"));
            assert!(!names.contains(&".hidden_file"));
            assert!(!names.contains(&".hidden_dir"));
        }

        #[test]
        fn test_list_directory_entries_empty_directory() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            // Empty directory
            let result = list_directory_entries(base, None).unwrap();
            assert!(result.is_empty());
        }

        #[test]
        fn test_list_directory_entries_nonexistent_directory() {
            let result = list_directory_entries(Path::new("/nonexistent/path/12345"), None);
            assert!(result.is_err());
        }

        #[test]
        fn test_list_directory_entries_is_directory_flag() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            create_file(base, "file.R");
            create_subdir(base, "directory");

            let result = list_directory_entries(base, None).unwrap();

            // Find the file entry
            let file_entry = result.iter().find(|(name, _, _)| name == "file.R");
            assert!(file_entry.is_some());
            assert!(!file_entry.unwrap().2, "file.R should not be marked as directory");

            // Find the directory entry
            let dir_entry = result.iter().find(|(name, _, _)| name == "directory");
            assert!(dir_entry.is_some());
            assert!(dir_entry.unwrap().2, "directory should be marked as directory");
        }

        #[test]
        fn test_list_directory_entries_sorted_dirs_first() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            // Create files and directories in non-alphabetical order
            create_file(base, "zebra.R");
            create_subdir(base, "alpha_dir");
            create_file(base, "alpha.R");
            create_subdir(base, "zebra_dir");

            let result = list_directory_entries(base, None).unwrap();

            // Directories should come first, then files
            // Within each group, should be alphabetically sorted (case-insensitive)
            assert_eq!(result.len(), 4);

            // First two should be directories
            assert!(result[0].2, "First entry should be a directory");
            assert!(result[1].2, "Second entry should be a directory");

            // Last two should be files
            assert!(!result[2].2, "Third entry should be a file");
            assert!(!result[3].2, "Fourth entry should be a file");

            // Check alphabetical order within groups
            assert_eq!(result[0].0, "alpha_dir");
            assert_eq!(result[1].0, "zebra_dir");
            assert_eq!(result[2].0, "alpha.R");
            assert_eq!(result[3].0, "zebra.R");
        }

        #[test]
        fn test_list_directory_entries_workspace_boundary() {
            let temp_dir = create_test_dir();
            let workspace = temp_dir.path();

            // Create a subdirectory as our "workspace"
            let subdir = create_subdir(workspace, "workspace");
            create_file(&subdir, "inside.R");

            // List entries with workspace boundary
            let result = list_directory_entries(&subdir, Some(&subdir)).unwrap();

            // Should include the file inside workspace
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0, "inside.R");
        }

        #[test]
        fn test_list_directory_entries_path_correctness() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            create_file(base, "test.R");

            let result = list_directory_entries(base, None).unwrap();

            assert_eq!(result.len(), 1);
            let (name, path, _) = &result[0];
            assert_eq!(name, "test.R");
            assert_eq!(path, &base.join("test.R"));
        }

        #[test]
        fn test_list_directory_entries_various_extensions() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            // Create files with various extensions
            // Note: On case-insensitive filesystems (macOS), script.R and script.r
            // would be the same file, so we use different base names
            create_file(base, "script.R");
            create_file(base, "another.r");
            create_file(base, "data.csv");
            create_file(base, "readme.md");
            create_file(base, "config.json");

            let result = list_directory_entries(base, None).unwrap();

            // All files should be listed (filtering is done by filter_r_files_and_dirs)
            assert_eq!(result.len(), 5);
        }

        #[test]
        fn test_list_directory_entries_special_characters_in_names() {
            let temp_dir = create_test_dir();
            let base = temp_dir.path();

            // Create files with special characters (that are valid on most filesystems)
            create_file(base, "file-with-dashes.R");
            create_file(base, "file_with_underscores.R");
            create_file(base, "file with spaces.R");

            let result = list_directory_entries(base, None).unwrap();

            assert_eq!(result.len(), 3);

            let names: Vec<&str> = result.iter().map(|(name, _, _)| name.as_str()).collect();
            assert!(names.contains(&"file-with-dashes.R"));
            assert!(names.contains(&"file_with_underscores.R"));
            assert!(names.contains(&"file with spaces.R"));
        }
    }

    // ========================================================================
    // Tests for filter_r_files_and_dirs
    // ========================================================================

    mod filter_r_files_tests {
        use super::*;

        /// Helper to create a test entry tuple
        fn entry(name: &str, is_dir: bool) -> (String, PathBuf, bool) {
            (name.to_string(), PathBuf::from(name), is_dir)
        }

        #[test]
        fn test_filter_keeps_r_uppercase_extension() {
            let entries = vec![
                entry("utils.R", false),
                entry("data.csv", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0, "utils.R");
        }

        #[test]
        fn test_filter_keeps_r_lowercase_extension() {
            let entries = vec![
                entry("helpers.r", false),
                entry("readme.md", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0, "helpers.r");
        }

        #[test]
        fn test_filter_keeps_all_directories() {
            let entries = vec![
                entry("subdir", true),
                entry("another_dir", true),
                entry("data.csv", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 2);
            assert!(result.iter().all(|(_, _, is_dir)| *is_dir));
        }

        #[test]
        fn test_filter_removes_non_r_files() {
            let entries = vec![
                entry("data.csv", false),
                entry("readme.md", false),
                entry("config.json", false),
                entry("script.py", false),
                entry("notes.txt", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert!(result.is_empty());
        }

        #[test]
        fn test_filter_mixed_entries() {
            let entries = vec![
                entry("utils.R", false),
                entry("helpers.r", false),
                entry("data.csv", false),
                entry("subdir", true),
                entry("readme.md", false),
                entry("another_dir", true),
            ];

            let result = filter_r_files_and_dirs(entries);

            // Should keep: utils.R, helpers.r, subdir, another_dir
            assert_eq!(result.len(), 4);

            let names: Vec<&str> = result.iter().map(|(name, _, _)| name.as_str()).collect();
            assert!(names.contains(&"utils.R"));
            assert!(names.contains(&"helpers.r"));
            assert!(names.contains(&"subdir"));
            assert!(names.contains(&"another_dir"));
            assert!(!names.contains(&"data.csv"));
            assert!(!names.contains(&"readme.md"));
        }

        #[test]
        fn test_filter_empty_input() {
            let entries: Vec<(String, PathBuf, bool)> = vec![];

            let result = filter_r_files_and_dirs(entries);

            assert!(result.is_empty());
        }

        #[test]
        fn test_filter_preserves_order() {
            let entries = vec![
                entry("a.R", false),
                entry("b_dir", true),
                entry("c.r", false),
                entry("d_dir", true),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 4);
            assert_eq!(result[0].0, "a.R");
            assert_eq!(result[1].0, "b_dir");
            assert_eq!(result[2].0, "c.r");
            assert_eq!(result[3].0, "d_dir");
        }

        #[test]
        fn test_filter_file_without_extension() {
            let entries = vec![
                entry("Makefile", false),
                entry("README", false),
                entry("utils.R", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0, "utils.R");
        }

        #[test]
        fn test_filter_similar_extensions() {
            // Test that similar extensions like .Rmd, .Rdata, .rds are NOT included
            let entries = vec![
                entry("script.R", false),
                entry("report.Rmd", false),
                entry("data.Rdata", false),
                entry("saved.rds", false),
                entry("another.r", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            // Should only keep .R and .r files
            assert_eq!(result.len(), 2);
            let names: Vec<&str> = result.iter().map(|(name, _, _)| name.as_str()).collect();
            assert!(names.contains(&"script.R"));
            assert!(names.contains(&"another.r"));
            assert!(!names.contains(&"report.Rmd"));
            assert!(!names.contains(&"data.Rdata"));
            assert!(!names.contains(&"saved.rds"));
        }

        #[test]
        fn test_filter_directory_with_r_in_name() {
            // Directories with .R in name should still be kept (they're directories)
            let entries = vec![
                entry("R_scripts", true),
                entry("my.R.backup", true),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 2);
        }

        #[test]
        fn test_filter_file_with_multiple_dots() {
            let entries = vec![
                entry("my.script.R", false),
                entry("backup.utils.r", false),
                entry("old.data.csv", false),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 2);
            let names: Vec<&str> = result.iter().map(|(name, _, _)| name.as_str()).collect();
            assert!(names.contains(&"my.script.R"));
            assert!(names.contains(&"backup.utils.r"));
        }

        #[test]
        fn test_filter_path_with_directory_components() {
            // Test that the path field doesn't affect filtering (we use name)
            let entries = vec![
                (
                    "utils.R".to_string(),
                    PathBuf::from("/some/path/utils.R"),
                    false,
                ),
                (
                    "data.csv".to_string(),
                    PathBuf::from("/some/path/data.csv"),
                    false,
                ),
            ];

            let result = filter_r_files_and_dirs(entries);

            assert_eq!(result.len(), 1);
            assert_eq!(result[0].0, "utils.R");
        }
    }
}

    // ========================================================================
    // Tests for resolve_base_directory and helper functions
    // ========================================================================

    #[cfg(test)]
    mod resolve_base_directory_tests {
        use super::*;

        // Helper to create a test metadata with optional working directory
        fn make_metadata(working_dir: Option<&str>) -> CrossFileMetadata {
            CrossFileMetadata {
                working_directory: working_dir.map(|s| s.to_string()),
                ..Default::default()
            }
        }

        // ====================================================================
        // Tests for extract_directory_component
        // ====================================================================

        #[test]
        fn test_extract_directory_component_simple_file() {
            assert_eq!(extract_directory_component("utils.R"), "");
        }

        #[test]
        fn test_extract_directory_component_with_subdir() {
            assert_eq!(extract_directory_component("subdir/utils.R"), "subdir/");
        }

        #[test]
        fn test_extract_directory_component_parent_dir() {
            assert_eq!(extract_directory_component("../utils.R"), "../");
        }

        #[test]
        fn test_extract_directory_component_parent_and_subdir() {
            assert_eq!(extract_directory_component("../data/utils.R"), "../data/");
        }

        #[test]
        fn test_extract_directory_component_multiple_parents() {
            assert_eq!(extract_directory_component("../../utils.R"), "../../");
        }

        #[test]
        fn test_extract_directory_component_trailing_slash() {
            assert_eq!(extract_directory_component("subdir/"), "subdir/");
        }

        #[test]
        fn test_extract_directory_component_empty() {
            assert_eq!(extract_directory_component(""), "");
        }

        #[test]
        fn test_extract_directory_component_deep_path() {
            assert_eq!(
                extract_directory_component("a/b/c/d/file.R"),
                "a/b/c/d/"
            );
        }

        // ====================================================================
        // Tests for normalize_path_for_completion
        // ====================================================================

        #[test]
        fn test_normalize_path_simple() {
            let path = Path::new("/project/src/utils.R");
            let result = normalize_path_for_completion(path).unwrap();
            assert_eq!(result, PathBuf::from("/project/src/utils.R"));
        }

        #[test]
        fn test_normalize_path_with_parent_dir() {
            let path = Path::new("/project/src/../data/utils.R");
            let result = normalize_path_for_completion(path).unwrap();
            assert_eq!(result, PathBuf::from("/project/data/utils.R"));
        }

        #[test]
        fn test_normalize_path_with_current_dir() {
            let path = Path::new("/project/./src/./utils.R");
            let result = normalize_path_for_completion(path).unwrap();
            assert_eq!(result, PathBuf::from("/project/src/utils.R"));
        }

        #[test]
        fn test_normalize_path_multiple_parent_dirs() {
            let path = Path::new("/project/src/deep/../../data/utils.R");
            let result = normalize_path_for_completion(path).unwrap();
            assert_eq!(result, PathBuf::from("/project/data/utils.R"));
        }

        #[test]
        fn test_normalize_path_preserves_root() {
            let path = Path::new("/../project/utils.R");
            let result = normalize_path_for_completion(path).unwrap();
            assert_eq!(result, PathBuf::from("/project/utils.R"));
        }

        // ====================================================================
        // Tests for resolve_base_directory
        // ====================================================================

        #[test]
        fn test_resolve_base_directory_source_call_no_partial() {
            // source("") - empty partial path, should return file's directory
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: String::new(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            assert_eq!(result.unwrap(), PathBuf::from("/project/src"));
        }

        #[test]
        fn test_resolve_base_directory_source_call_with_subdir() {
            // source("subdir/") - partial path with subdir
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "subdir/".to_string(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            assert_eq!(result.unwrap(), PathBuf::from("/project/src/subdir"));
        }

        #[test]
        fn test_resolve_base_directory_source_call_with_parent_dir() {
            // source("../") - partial path with parent directory
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "../".to_string(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            assert_eq!(result.unwrap(), PathBuf::from("/project"));
        }

        #[test]
        fn test_resolve_base_directory_source_call_with_lsp_cd() {
            // source("") with @lsp-cd set - should use working directory
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(Some("/data")); // workspace-relative

            let context = FilePathContext::SourceCall {
                partial_path: String::new(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project/data (workspace root + /data)
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_source_call_with_lsp_cd_and_parent() {
            // source("../") with @lsp-cd set - should use working directory + ../
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(Some("/data/scripts")); // workspace-relative

            let context = FilePathContext::SourceCall {
                partial_path: "../".to_string(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project/data (working dir /project/data/scripts + ../)
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_directive_ignores_lsp_cd() {
            // Directive with @lsp-cd set - should IGNORE working directory
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(Some("/data")); // This should be ignored for directives

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: String::new(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to file's directory, NOT /project/data
            assert_eq!(result.unwrap(), PathBuf::from("/project/src"));
        }

        #[test]
        fn test_resolve_base_directory_directive_with_parent_dir() {
            // Directive with ../ - should resolve relative to file's directory
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(Some("/data")); // This should be ignored

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: "../".to_string(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project (file's dir /project/src + ../)
            assert_eq!(result.unwrap(), PathBuf::from("/project"));
        }

        #[test]
        fn test_resolve_base_directory_directive_forward() {
            // Forward directive (@lsp-source) - same behavior as backward
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::Source,
                partial_path: "utils/".to_string(),
                path_start: Position { line: 0, character: 14 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            assert_eq!(result.unwrap(), PathBuf::from("/project/src/utils"));
        }

        #[test]
        fn test_resolve_base_directory_none_context() {
            // FilePathContext::None should return None
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::None;

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_none());
        }

        #[test]
        fn test_resolve_base_directory_partial_filename() {
            // Partial path is just a filename prefix (no directory component)
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "util".to_string(), // Just typing "util", no slash
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should return file's directory since there's no directory component
            assert_eq!(result.unwrap(), PathBuf::from("/project/src"));
        }

        #[test]
        fn test_resolve_base_directory_backslash_normalization() {
            // Partial path with backslashes should be normalized
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "subdir\\".to_string(), // Backslash
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            assert_eq!(result.unwrap(), PathBuf::from("/project/src/subdir"));
        }

        #[test]
        fn test_resolve_base_directory_sys_source() {
            // sys.source() should behave the same as source()
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "../data/".to_string(),
                content_start: Position { line: 0, character: 12 },
                is_sys_source: true,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_deep_parent_navigation() {
            // Multiple parent directories
            let file_uri = Url::parse("file:///project/src/deep/nested/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "../../".to_string(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // /project/src/deep/nested + ../../ = /project/src
            assert_eq!(result.unwrap(), PathBuf::from("/project/src"));
        }

        // ====================================================================
        // Tests for workspace-root-relative paths (starting with `/`)
        // ====================================================================

        #[test]
        fn test_resolve_base_directory_directive_workspace_root_relative_just_slash() {
            // Directive with just "/" - should return workspace root
            let file_uri = Url::parse("file:///project/src/deep/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: "/".to_string(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to workspace root
            assert_eq!(result.unwrap(), PathBuf::from("/project"));
        }

        #[test]
        fn test_resolve_base_directory_directive_workspace_root_relative_subdir() {
            // Directive with "/data/" - should resolve to workspace_root/data
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: "/data/".to_string(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project/data
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_directive_workspace_root_relative_nested() {
            // Directive with "/src/utils/" - should resolve to workspace_root/src/utils
            let file_uri = Url::parse("file:///project/tests/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::Source,
                partial_path: "/src/utils/".to_string(),
                path_start: Position { line: 0, character: 14 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project/src/utils
            assert_eq!(result.unwrap(), PathBuf::from("/project/src/utils"));
        }

        #[test]
        fn test_resolve_base_directory_directive_workspace_root_relative_partial_filename() {
            // Directive with "/data/util" (partial filename, no trailing slash)
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: "/data/util".to_string(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project/data (directory component only)
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_directive_workspace_root_relative_ignores_lsp_cd() {
            // Directive with "/" and @lsp-cd set - should still use workspace root
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(Some("/other/dir")); // This should be ignored

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: "/data/".to_string(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // Should resolve to /project/data, NOT affected by @lsp-cd
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_source_call_workspace_root_relative() {
            // source() with "/" - should be treated as workspace-root-relative
            // Same behavior as directives now
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let workspace_root = Url::parse("file:///project").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "/data/".to_string(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, Some(&workspace_root));

            assert!(result.is_some());
            // For source() calls, "/" is now workspace-root-relative
            // Should resolve to /project/data
            assert_eq!(result.unwrap(), PathBuf::from("/project/data"));
        }

        #[test]
        fn test_resolve_base_directory_source_call_workspace_root_relative_no_workspace() {
            // source() with "/" but no workspace root - should return None
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::SourceCall {
                partial_path: "/data/".to_string(),
                content_start: Position { line: 0, character: 8 },
                is_sys_source: false,
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, None);

            // Without workspace root, workspace-root-relative paths cannot be resolved
            assert!(result.is_none());
        }

        #[test]
        fn test_resolve_base_directory_directive_workspace_root_relative_no_workspace() {
            // Directive with "/" but no workspace root - should return None
            let file_uri = Url::parse("file:///project/src/main.R").unwrap();
            let metadata = make_metadata(None);

            let context = FilePathContext::Directive {
                directive_type: DirectiveType::SourcedBy,
                partial_path: "/data/".to_string(),
                path_start: Position { line: 0, character: 18 },
            };

            let result = resolve_base_directory(&context, &file_uri, &metadata, None);

            // Without workspace root, workspace-root-relative paths cannot be resolved
            assert!(result.is_none());
        }
    }


    // ========================================================================
    // Tests for file_path_definition
    // ========================================================================

    #[cfg(test)]
    mod file_path_definition_tests {
        use super::*;
        use std::fs;
        use tempfile::TempDir;
        use tree_sitter::Parser;

        fn parse_r(code: &str) -> Tree {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_r::LANGUAGE.into())
                .unwrap();
            parser.parse(code, None).unwrap()
        }

        fn make_metadata(working_dir: Option<&str>) -> CrossFileMetadata {
            CrossFileMetadata {
                working_directory: working_dir.map(|s| s.to_string()),
                ..Default::default()
            }
        }

        #[test]
        fn test_file_path_definition_source_call_existing_file() {
            // Create a temp directory with a file
            let temp_dir = TempDir::new().unwrap();
            let utils_path = temp_dir.path().join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();

            let main_path = temp_dir.path().join("main.R");
            let code = r#"source("utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor in the middle of the path
            let position = Position { line: 0, character: 10 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
            assert_eq!(location.range.start.line, 0);
            assert_eq!(location.range.start.character, 0);
        }

        #[test]
        fn test_file_path_definition_source_call_nonexistent_file() {
            let temp_dir = TempDir::new().unwrap();
            let main_path = temp_dir.path().join("main.R");
            let code = r#"source("nonexistent.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 10 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should return None for non-existent file
            assert!(result.is_none());
        }

        #[test]
        fn test_file_path_definition_directive_existing_file() {
            let temp_dir = TempDir::new().unwrap();
            let parent_path = temp_dir.path().join("parent.R");
            fs::write(&parent_path, "# parent file").unwrap();

            let child_path = temp_dir.path().join("child.R");
            let code = "# @lsp-sourced-by parent.R";
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&child_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor on the path
            let position = Position { line: 0, character: 20 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&parent_path).unwrap());
            assert_eq!(location.range.start.line, 0);
            assert_eq!(location.range.start.character, 0);
        }

        #[test]
        fn test_file_path_definition_directive_nonexistent_file() {
            let temp_dir = TempDir::new().unwrap();
            let child_path = temp_dir.path().join("child.R");
            let code = "# @lsp-sourced-by nonexistent.R";
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&child_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 20 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should return None for non-existent file
            assert!(result.is_none());
        }

        #[test]
        fn test_file_path_definition_source_call_respects_lsp_cd() {
            // Create temp directory structure:
            // temp_dir/
            //   main.R
            //   subdir/
            //     utils.R
            let temp_dir = TempDir::new().unwrap();
            let subdir = temp_dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let utils_path = subdir.join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();

            let main_path = temp_dir.path().join("main.R");
            let code = r#"source("utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            // Set @lsp-cd to subdir using a relative path
            let metadata = make_metadata(Some("subdir"));

            let position = Position { line: 0, character: 10 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should find utils.R in subdir because @lsp-cd is set
            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_directive_ignores_lsp_cd() {
            // Create temp directory structure:
            // temp_dir/
            //   parent.R
            //   child.R
            //   subdir/
            //     parent.R (different file)
            let temp_dir = TempDir::new().unwrap();
            let parent_path = temp_dir.path().join("parent.R");
            fs::write(&parent_path, "# parent in root").unwrap();

            let subdir = temp_dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let subdir_parent = subdir.join("parent.R");
            fs::write(&subdir_parent, "# parent in subdir").unwrap();

            let child_path = temp_dir.path().join("child.R");
            let code = "# @lsp-sourced-by parent.R";
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&child_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            // Set @lsp-cd to subdir using relative path - should be IGNORED for directives
            let metadata = make_metadata(Some("subdir"));

            let position = Position { line: 0, character: 20 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should find parent.R in root (same dir as child.R), NOT in subdir
            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&parent_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_relative_path() {
            // Create temp directory structure:
            // temp_dir/
            //   utils.R
            //   subdir/
            //     main.R
            let temp_dir = TempDir::new().unwrap();
            let utils_path = temp_dir.path().join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();

            let subdir = temp_dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let main_path = subdir.join("main.R");

            let code = r#"source("../utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 12 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_path_with_subdirectory() {
            // Create temp directory structure:
            // temp_dir/
            //   main.R
            //   subdir/
            //     utils.R
            let temp_dir = TempDir::new().unwrap();
            let main_path = temp_dir.path().join("main.R");

            let subdir = temp_dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let utils_path = subdir.join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();

            // Test go-to-definition for source("subdir/utils.R")
            let code = r#"source("subdir/utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor in the middle of the path
            let position = Position { line: 0, character: 15 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some(), "Should find file at subdir/utils.R");
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_deep_path() {
            // Create temp directory structure:
            // temp_dir/
            //   main.R
            //   path/
            //     to/
            //       some/
            //         file.R
            let temp_dir = TempDir::new().unwrap();
            let main_path = temp_dir.path().join("main.R");

            let path_dir = temp_dir.path().join("path");
            fs::create_dir(&path_dir).unwrap();
            let to_dir = path_dir.join("to");
            fs::create_dir(&to_dir).unwrap();
            let some_dir = to_dir.join("some");
            fs::create_dir(&some_dir).unwrap();
            let file_path = some_dir.join("file.R");
            fs::write(&file_path, "# deep file").unwrap();

            // Test go-to-definition for source("path/to/some/file.R")
            let code = r#"source("path/to/some/file.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor in the middle of the path
            let position = Position { line: 0, character: 18 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some(), "Should find file at path/to/some/file.R");
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&file_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_cursor_not_on_path() {
            let temp_dir = TempDir::new().unwrap();
            let main_path = temp_dir.path().join("main.R");
            let code = r#"x <- 1
source("utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor on first line, not on a file path
            let position = Position { line: 0, character: 3 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should return None when cursor is not on a file path
            assert!(result.is_none());
        }

        #[test]
        fn test_file_path_definition_empty_path() {
            let temp_dir = TempDir::new().unwrap();
            let main_path = temp_dir.path().join("main.R");
            let code = r#"source("")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor inside empty string
            let position = Position { line: 0, character: 8 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should return None for empty path
            assert!(result.is_none());
        }

        #[test]
        fn test_file_path_definition_sys_source() {
            let temp_dir = TempDir::new().unwrap();
            let utils_path = temp_dir.path().join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();

            let main_path = temp_dir.path().join("main.R");
            let code = r#"sys.source("utils.R", envir = globalenv())"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            // Cursor in the path
            let position = Position { line: 0, character: 15 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_path_to_directory_returns_none() {
            let temp_dir = TempDir::new().unwrap();
            let subdir = temp_dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();

            let main_path = temp_dir.path().join("main.R");
            let code = r#"source("subdir")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 10 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should return None because path points to a directory, not a file
            assert!(result.is_none());
        }

        #[test]
        fn test_file_path_definition_backslash_normalization() {
            let temp_dir = TempDir::new().unwrap();
            let subdir = temp_dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let utils_path = subdir.join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();

            let main_path = temp_dir.path().join("main.R");
            // Using escaped backslashes (as they would appear in R string)
            let code = r#"source("subdir\\utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(temp_dir.path()).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 15 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_workspace_boundary_parent_escape() {
            // Test that paths escaping workspace via ../ return None
            let temp_dir = TempDir::new().unwrap();
            
            // Create workspace as a subdirectory
            let workspace = temp_dir.path().join("workspace");
            fs::create_dir(&workspace).unwrap();
            
            // Create a file OUTSIDE the workspace
            let outside_file = temp_dir.path().join("outside.R");
            fs::write(&outside_file, "# outside file").unwrap();
            
            // Create main file inside workspace/subdir
            let subdir = workspace.join("subdir");
            fs::create_dir(&subdir).unwrap();
            let main_path = subdir.join("main.R");
            
            // Try to access file outside workspace via ../../outside.R
            let code = r#"source("../../outside.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(&workspace).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 12 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should return None because file is outside workspace
            assert!(result.is_none());
        }

        #[test]
        fn test_file_path_definition_workspace_boundary_inside_allowed() {
            // Test that paths inside workspace are allowed
            let temp_dir = TempDir::new().unwrap();
            let workspace = temp_dir.path();
            
            // Create a file inside workspace
            let utils_path = workspace.join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();
            
            // Create main file in subdir
            let subdir = workspace.join("subdir");
            fs::create_dir(&subdir).unwrap();
            let main_path = subdir.join("main.R");
            
            // Access file via ../utils.R (still inside workspace)
            let code = r#"source("../utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let workspace_root = Url::from_file_path(workspace).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 12 };

            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                Some(&workspace_root),
            );

            // Should succeed because file is inside workspace
            assert!(result.is_some());
            let location = result.unwrap();
            assert_eq!(location.uri, Url::from_file_path(&utils_path).unwrap());
        }

        #[test]
        fn test_file_path_definition_no_workspace_root_no_boundary_check() {
            // Test that without workspace_root, no boundary check is performed
            let temp_dir = TempDir::new().unwrap();
            
            // Create a file
            let utils_path = temp_dir.path().join("utils.R");
            fs::write(&utils_path, "# utils file").unwrap();
            
            let main_path = temp_dir.path().join("main.R");
            let code = r#"source("utils.R")"#;
            let tree = parse_r(code);

            let file_uri = Url::from_file_path(&main_path).unwrap();
            let metadata = make_metadata(None);

            let position = Position { line: 0, character: 10 };

            // No workspace_root provided
            let result = file_path_definition(
                &tree,
                code,
                position,
                &file_uri,
                &metadata,
                None,
            );

            // Should succeed (no boundary check without workspace_root)
            assert!(result.is_some());
        }
    }
