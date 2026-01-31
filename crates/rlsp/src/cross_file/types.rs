//
// cross_file/types.rs
//
// Core types for cross-file awareness
//

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tower_lsp::lsp_types::Url;

/// Complete cross-file metadata for a document
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrossFileMetadata {
    /// Backward directives (this file is sourced by others)
    pub sourced_by: Vec<BackwardDirective>,
    /// Forward directives and detected source() calls
    pub sources: Vec<ForwardSource>,
    /// Working directory override
    pub working_directory: Option<String>,
    /// Lines with @lsp-ignore (0-based)
    pub ignored_lines: HashSet<u32>,
    /// Lines following @lsp-ignore-next (0-based)
    pub ignored_next_lines: HashSet<u32>,
}

/// A backward directive declaring this file is sourced by another
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackwardDirective {
    pub path: String,
    pub call_site: CallSiteSpec,
    /// 0-based line where the directive appears
    pub directive_line: u32,
}

/// A forward source (directive or detected source() call)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForwardSource {
    pub path: String,
    /// 0-based line
    pub line: u32,
    /// 0-based UTF-16 column
    pub column: u32,
    /// true if @lsp-source directive, false if detected source()
    pub is_directive: bool,
    /// source(..., local = TRUE)
    pub local: bool,
    /// source(..., chdir = TRUE)
    pub chdir: bool,
    /// true for sys.source(), false for source()
    pub is_sys_source: bool,
    /// For sys.source: true if envir=globalenv()/.GlobalEnv, false otherwise
    /// When false for sys.source, symbols are NOT inherited (treated as local)
    pub sys_source_global_env: bool,
}

impl ForwardSource {
    /// Check if symbols from this source should be inherited
    /// Returns false for local=TRUE or sys.source with non-global env
    pub fn inherits_symbols(&self) -> bool {
        if self.local {
            return false;
        }
        if self.is_sys_source && !self.sys_source_global_env {
            return false;
        }
        true
    }
}

/// Canonical key for edge deduplication
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForwardSourceKey {
    pub resolved_uri: Url,
    pub call_site_line: u32,
    pub call_site_column: u32,
    pub local: bool,
    pub chdir: bool,
    pub is_sys_source: bool,
}

impl ForwardSource {
    /// Create a canonical key for deduplication (requires resolved URI)
    pub fn to_key(&self, resolved_uri: Url) -> ForwardSourceKey {
        ForwardSourceKey {
            resolved_uri,
            call_site_line: self.line,
            call_site_column: self.column,
            local: self.local,
            chdir: self.chdir,
            is_sys_source: self.is_sys_source,
        }
    }
}

/// Call site specification for backward directives
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CallSiteSpec {
    /// Use configuration default
    Default,
    /// Explicit line number (0-based internally, converted from 1-based user input)
    Line(u32),
    /// Pattern to match in parent file
    Match(String),
}

impl Default for CallSiteSpec {
    fn default() -> Self {
        CallSiteSpec::Default
    }
}

/// Convert a byte offset to UTF-16 column for a given line.
/// Handles multi-byte UTF-8 characters correctly for LSP position conversion.
pub fn byte_offset_to_utf16_column(line_text: &str, byte_offset_in_line: usize) -> u32 {
    let prefix = &line_text[..byte_offset_in_line.min(line_text.len())];
    prefix.encode_utf16().count() as u32
}

/// Convert a tree-sitter Point to LSP Position with correct UTF-16 column.
/// Ensures proper handling of multi-byte characters in LSP protocol communication.
pub fn tree_sitter_point_to_lsp_position(
    point: tree_sitter::Point,
    line_text: &str,
) -> tower_lsp::lsp_types::Position {
    let column = byte_offset_to_utf16_column(line_text, point.column);
    tower_lsp::lsp_types::Position {
        line: point.row as u32,
        character: column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_forward_source_to_key() {
        let source = ForwardSource {
            path: "test.R".to_string(),
            line: 10,
            column: 5,
            is_directive: false,
            local: true,
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: true,
        };
        let uri = Url::parse("file:///test.R").unwrap();
        let key = source.to_key(uri.clone());
        
        assert_eq!(key.resolved_uri, uri);
        assert_eq!(key.call_site_line, 10);
        assert_eq!(key.call_site_column, 5);
        assert!(key.local);
        assert!(!key.chdir);
        assert!(!key.is_sys_source);
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
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            working_directory: Some("/data".to_string()),
            ignored_lines: HashSet::from([10, 20]),
            ignored_next_lines: HashSet::from([15]),
        };
        
        // Round-trip serialization
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: CrossFileMetadata = serde_json::from_str(&json).unwrap();
        
        assert_eq!(parsed.sourced_by.len(), 1);
        assert_eq!(parsed.sources.len(), 1);
        assert_eq!(parsed.working_directory, Some("/data".to_string()));
        assert!(parsed.ignored_lines.contains(&10));
        assert!(parsed.ignored_next_lines.contains(&15));
    }

    #[test]
    fn test_inherits_symbols_local_true() {
        let source = ForwardSource {
            path: "test.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            local: true,
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: true,
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
            local: false,
            chdir: false,
            is_sys_source: true,
            sys_source_global_env: false,
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
            local: false,
            chdir: false,
            is_sys_source: true,
            sys_source_global_env: true,
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
            local: false,
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: true,
        };
        assert!(source.inherits_symbols());
    }
}