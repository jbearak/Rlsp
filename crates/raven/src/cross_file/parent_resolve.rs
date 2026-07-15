//
// cross_file/parent_resolve.rs
//
// Parent resolution for cross-file awareness
//

use super::types::byte_offset_to_utf16_column;

/// Resolve a match= pattern in parent content to find the call site.
/// Returns (line, utf16_column) of the first match on a line containing source()/sys.source() to child.
/// Falls back to first match on any line if no source() call found.
pub fn resolve_match_pattern(
    parent_content: &str,
    pattern: &str,
    child_path: &str,
) -> Option<(u32, u32)> {
    // Extract just the filename from child_path for matching
    let child_filename = std::path::Path::new(child_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(child_path);

    let mut first_match: Option<(u32, u32)> = None;

    for (line_num, line) in parent_content.lines().enumerate() {
        if let Some(byte_offset) = line.find(pattern) {
            let utf16_col = byte_offset_to_utf16_column(line, byte_offset);
            let pos = (line_num as u32, utf16_col);

            // Check if this line contains a source() or sys.source() call to the child
            let has_source_call = (line.contains("source(") || line.contains("sys.source("))
                && (line.contains(child_path) || line.contains(child_filename));

            if has_source_call {
                return Some(pos);
            }

            // Remember first match as fallback
            if first_match.is_none() {
                first_match = Some(pos);
            }
        }
    }

    first_match
}

/// Infer call site by scanning parent content for source()/sys.source() calls to child.
/// Used when call_site is Default and no reverse edge exists.
/// Returns (line, utf16_column) of the first matching source() call.
pub fn infer_call_site_from_parent(parent_content: &str, child_path: &str) -> Option<(u32, u32)> {
    let child_filename = std::path::Path::new(child_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(child_path);

    // Helper to check if a path in source() matches our child path
    // Accepts exact matches or paths that end with our filename/path
    let path_matches = |quoted_path: &str| -> bool {
        quoted_path == child_path
            || quoted_path == child_filename
            || quoted_path.ends_with(&format!("/{}", child_filename))
            || quoted_path.ends_with(&format!("/{}", child_path))
    };

    for (line_num, line) in parent_content.lines().enumerate() {
        // Look for sys.source() first (more specific), then source()
        let call_start = if let Some(pos) = line.find("sys.source(") {
            Some(pos)
        } else {
            line.find("source(")
        };

        if let Some(start) = call_start {
            // Check if this call references the child path (string literal)
            let after_call = &line[start..];

            // Extract paths from quoted strings and check for matches
            let mut matched = false;

            // Check double-quoted paths
            for part in after_call.split('"') {
                // Every other part (1st, 3rd, etc.) is the content between quotes
                if path_matches(part) {
                    matched = true;
                    break;
                }
            }

            // Check single-quoted paths if not yet matched
            if !matched {
                for part in after_call.split('\'') {
                    if path_matches(part) {
                        matched = true;
                        break;
                    }
                }
            }

            if matched {
                let utf16_col = byte_offset_to_utf16_column(line, start);
                return Some((line_num as u32, utf16_col));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_match_pattern_basic() {
        let parent_content = r#"x <- 1
source("child.R")
y <- 2"#;
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_resolve_match_pattern_with_source_call() {
        let parent_content = r#"# source( comment
x <- 1
source("child.R")
y <- 2"#;
        // Should prefer line with actual source() call to child
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        assert_eq!(result, Some((2, 0)));
    }

    #[test]
    fn test_infer_call_site_matches_computed_file_path_parent() {
        // A parent using the computed-path idiom (issue #638): the child
        // filename still appears as a quoted segment on the call line, so the
        // text scan must find the call site.
        let parent_content = r#"repo_root <- normalizePath(file.path("..", ".."))
source(file.path(repo_root, "scripts/helpers.R"))"#;
        let result = infer_call_site_from_parent(parent_content, "scripts/helpers.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_resolve_match_pattern_fallback() {
        let parent_content = r#"# source( comment
x <- 1
y <- 2"#;
        // No source() call to child, falls back to first match
        let result = resolve_match_pattern(parent_content, "source(", "other.R");
        assert_eq!(result, Some((0, 2))); // "# source(" at column 2
    }

    #[test]
    fn test_resolve_match_pattern_not_found() {
        let parent_content = "x <- 1\ny <- 2";
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_match_pattern_utf16_column() {
        // Test with Unicode: 🎉 is 4 bytes UTF-8, 2 UTF-16 code units
        let parent_content = "🎉source(\"child.R\")";
        let result = resolve_match_pattern(parent_content, "source(", "child.R");
        // "🎉" is 2 UTF-16 units, so source( starts at column 2
        assert_eq!(result, Some((0, 2)));
    }

    #[test]
    fn test_infer_call_site_basic() {
        let parent_content = r#"x <- 1
source("child.R")
y <- 2"#;
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_infer_call_site_sys_source() {
        let parent_content = r#"x <- 1
sys.source("child.R", envir = globalenv())
y <- 2"#;
        let result = infer_call_site_from_parent(parent_content, "child.R");
        // sys.source( starts at column 0
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_infer_call_site_named_arg() {
        let parent_content = r#"source(file = "child.R")"#;
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_single_quotes() {
        let parent_content = "source('child.R')";
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_not_found() {
        let parent_content = "source(\"other.R\")";
        let result = infer_call_site_from_parent(parent_content, "child.R");
        assert_eq!(result, None);
    }

    #[test]
    fn test_infer_call_site_filename_only() {
        // Should match by filename even if directive has relative path
        let parent_content = "source(\"child.R\")";
        let result = infer_call_site_from_parent(parent_content, "../subdir/child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_subdir_path() {
        // Should match source("subdir/child.R") when child_path is "subdir/child.R"
        let parent_content = "x <- 1\nsource(\"subdir/child.R\")\ny <- 2";
        let result = infer_call_site_from_parent(parent_content, "subdir/child.R");
        assert_eq!(result, Some((1, 0)));
    }

    #[test]
    fn test_infer_call_site_nested_subdir_path() {
        // Should match when source path ends with our child path
        let parent_content = "source(\"R/subdir/child.R\")";
        let result = infer_call_site_from_parent(parent_content, "subdir/child.R");
        assert_eq!(result, Some((0, 0)));
    }

    #[test]
    fn test_infer_call_site_relative_path_match() {
        // Should match relative path in source call
        let parent_content = "source(\"./subdir/child.R\")";
        // This should match because the path ends with "subdir/child.R" after removing "./"
        let result = infer_call_site_from_parent(parent_content, "subdir/child.R");
        assert_eq!(result, Some((0, 0)));
    }
}
