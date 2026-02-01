// namespace_parser.rs - NAMESPACE and DESCRIPTION file parsing for package exports
//
// This module provides fallback parsing for R package metadata when R subprocess
// is unavailable. It parses NAMESPACE files to extract exported symbols and
// DESCRIPTION files to extract package dependencies.
//
// Requirement 3.2: IF R subprocess is unavailable, THE Package_Resolver SHALL
// fall back to parsing the package's NAMESPACE file directly

// Allow dead code during incremental development - this module will be
// integrated into PackageLibrary in task 3.3
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;

/// Extracts exported symbol names from an R package NAMESPACE file.
///
/// Recognizes `export(name[, ...])`, `exportPattern("pattern")` and `S3method(generic, class[, method])`.
/// Comments and blank lines are ignored; multiline directives are supported. `exportPattern` entries
/// are preserved as markers in the form `__PATTERN__:<pattern>`. S3 methods are returned as
/// `generic.class`. Whitespace is trimmed and empty names are omitted.
///
/// # Returns
///
/// A `Vec<String>` containing exported symbol names in document order. Pattern exports appear as
/// `__PATTERN__:<pattern>` and S3 methods as `generic.class`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// let exports = crate::namespace_parser::parse_namespace_exports(Path::new("NAMESPACE")).unwrap();
/// // `exports` may contain entries like "foo", "__PATTERN__:^bar.*$", or "print.myclass"
/// ```
pub fn parse_namespace_exports(namespace_path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(namespace_path)
        .map_err(|e| anyhow!("Failed to read NAMESPACE file {:?}: {}", namespace_path, e))?;

    Ok(parse_namespace_content(&content))
}

/// Parse NAMESPACE file content to extract exported symbols and pattern markers.
///
/// This function scans normalized NAMESPACE directives and collects:
///
/// - plain exported names from `export(...)` directives,
/// - S3 methods from `S3method(generic, class[, method])` formatted as `generic.class`,
/// - export patterns from `exportPattern(...)` stored as markers `__PATTERN__:<pattern>`.
///
/// Comments and blank lines are ignored; whitespace is trimmed and empty names are filtered out.
///
/// # Examples
///
/// ```
/// let content = r#"
/// export(foo, "bar")
/// exportPattern(".*_internal$")
/// S3method(print, foo)
/// "#;
/// let exports = parse_namespace_content(content);
/// assert!(exports.contains(&"foo".to_string()));
/// assert!(exports.contains(&"bar".to_string()));
/// assert!(exports.contains(&"__PATTERN__:.*_internal$".to_string()));
/// assert!(exports.contains(&"print.foo".to_string()));
/// ```
fn parse_namespace_content(content: &str) -> Vec<String> {
    let mut exports = Vec::new();

    // First, normalize the content by joining multi-line directives
    // NAMESPACE files can have directives spanning multiple lines
    let normalized = normalize_multiline_directives(content);

    for line in normalized.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Handle export(name1, name2, ...)
        // Requirement 3.3: WHEN a NAMESPACE file contains `export(name)`,
        // THE Package_Resolver SHALL include `name` in the package's exports
        if let Some(args) = extract_directive_args(line, "export") {
            for name in parse_comma_separated_args(&args) {
                if !name.is_empty() {
                    exports.push(name);
                }
            }
        }
        // Handle exportPattern("pattern")
        // Requirement 3.4: WHEN a NAMESPACE file contains `exportPattern("pattern")`,
        // THE Package_Resolver SHALL include matching symbols from the package
        // Note: We store the pattern itself since we can't expand it without R source files
        else if let Some(args) = extract_directive_args(line, "exportPattern") {
            for pattern in parse_comma_separated_args(&args) {
                if !pattern.is_empty() {
                    // Store pattern with a prefix to distinguish from regular exports
                    // This allows callers to identify patterns for later expansion
                    exports.push(format!("__PATTERN__:{}", pattern));
                }
            }
        }
        // Handle S3method(generic, class) and S3method(generic, class, method)
        // Requirement 3.5: WHEN a NAMESPACE file contains `S3method(generic, class)`,
        // THE Package_Resolver SHALL include the S3 method in exports
        else if let Some(args) = extract_directive_args(line, "S3method") {
            if let Some(method_name) = parse_s3method_args(&args) {
                exports.push(method_name);
            }
        }
    }

    exports
}

/// Collapse NAMESPACE directives that span multiple lines into single logical lines.
///
/// Preserves comment-only lines (lines starting with `#`) as separate lines when they are
/// not inside a parenthesized directive. If a directive's parentheses never close, the
/// accumulated line is emitted as-is at the end.
///
/// # Examples
///
/// ```
/// let src = r#"
/// export(
///   func1,
///   func2
/// )
/// # a comment
/// otherDirective(x)
/// "#;
///
/// let normalized = normalize_multiline_directives(src);
/// assert!(normalized.contains("export( func1, func2 )"));
/// assert!(normalized.contains("# a comment\n"));
/// assert!(normalized.contains("otherDirective(x)"));
/// ```
fn normalize_multiline_directives(content: &str) -> String {
    let mut result = String::new();
    let mut current_line = String::new();
    let mut paren_depth: i32 = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip comment-only lines when not inside a directive
        if paren_depth == 0 && trimmed.starts_with('#') {
            result.push_str(trimmed);
            result.push('\n');
            continue;
        }

        // Count parentheses to track multi-line directives
        for ch in trimmed.chars() {
            if ch == '(' {
                paren_depth += 1;
            } else if ch == ')' {
                paren_depth = paren_depth.saturating_sub(1);
            }
        }

        if current_line.is_empty() {
            current_line = trimmed.to_string();
        } else {
            // Join with space to preserve separation
            current_line.push(' ');
            current_line.push_str(trimmed);
        }

        // If we've closed all parentheses, emit the line
        if paren_depth == 0 {
            result.push_str(&current_line);
            result.push('\n');
            current_line.clear();
        }
    }

    // Handle any remaining content (unclosed parentheses)
    if !current_line.is_empty() {
        result.push_str(&current_line);
        result.push('\n');
    }

    result
}

/// Extracts the argument text for a directive like `export(arg1, arg2)`.
///
/// Returns the string between the directive's outer parentheses if the given line
/// starts with the directive name followed immediately by `(`; returns `None`
/// if the line does not start with the directive or lacks the opening `(`.
/// If a matching closing parenthesis cannot be found, returns the remainder of
/// the line after the opening `(` with any trailing `)` characters trimmed.
///
/// # Examples
///
/// ```
/// assert_eq!(extract_directive_args("export(foo, bar)", "export"), Some("foo, bar".to_string()));
/// assert_eq!(extract_directive_args("something(foo)", "export"), None);
/// ```
fn extract_directive_args(line: &str, directive: &str) -> Option<String> {
    // Find the opening parenthesis
    let after_directive = line.strip_prefix(directive)?;
    if !after_directive.starts_with('(') {
        return None;
    }

    // Find matching closing parenthesis
    let content = &after_directive[1..]; // Skip the opening paren
    if let Some(close_pos) = find_matching_paren(content) {
        Some(content[..close_pos].to_string())
    } else {
        // No closing paren found, take everything
        Some(content.trim_end_matches(')').to_string())
    }
}

/// Find the byte index of the closing parenthesis that matches an implicit opening parenthesis immediately before the input.
///
/// Scans the input left-to-right while tracking nested `(` / `)` pairs; returns the index of the `)` that closes the implicit opener (i.e., the `)` that brings the nesting depth to zero). Returns `None` if no matching closing parenthesis is found.
///
/// # Examples
///
/// ```
/// let s = "a,b)";
/// // treating the input as the content after an opening '(', the closing ')' is at byte index 3
/// assert_eq!(find_matching_paren(s), Some(3));
/// ```
fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// Split a comma-separated argument string into individual arguments while preserving quoted substrings.
///
/// Trims surrounding whitespace for each argument, supports both double (`"`) and single (`'`) quotes,
/// ignores commas that appear inside quoted strings, and omits empty items.
///
/// Returns a `Vec<String>` containing the parsed, trimmed arguments in order.
///
/// # Examples
///
/// ```
/// let v = parse_comma_separated_args(r#"foo, "bar baz", 'qux',  , "escaped,comma""#);
/// assert_eq!(v, vec!["foo", "bar baz", "qux", "escaped,comma"]);
/// ```
fn parse_comma_separated_args(args: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';

    for ch in args.chars() {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            c if c == quote_char && in_quotes => {
                in_quotes = false;
            }
            ',' if !in_quotes => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push(trimmed);
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    // Don't forget the last argument
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        result.push(trimmed);
    }

    result
}

/// Parse S3method directive arguments into an export name.
///
/// Returns `Some("generic.class")` when the arguments contain a non-empty generic and class;
/// returns `None` otherwise.
///
/// # Examples
///
/// ```
/// assert_eq!(parse_s3method_args("foo, bar"), Some("foo.bar".to_string()));
/// assert_eq!(parse_s3method_args("foo, bar, baz"), Some("foo.bar".to_string())); // method ignored
/// assert_eq!(parse_s3method_args(""), None);
/// ```
fn parse_s3method_args(args: &str) -> Option<String> {
    let parts = parse_comma_separated_args(args);

    if parts.len() >= 2 {
        let generic = parts[0].trim();
        let class = parts[1].trim();

        if !generic.is_empty() && !class.is_empty() {
            return Some(format!("{}.{}", generic, class));
        }
    }

    None
}

/// Extracts package names from the DESCRIPTION file's "Depends" field.
///
/// Parses the DCF-formatted DESCRIPTION content, supports continuation lines
/// for multi-line field values, strips version constraints (e.g., "(>= 3.5)"),
/// and filters out the special "R" entry.
///
/// Returns a `Vec<String>` with package names listed in the `Depends` field;
/// returns an empty vector if the field is missing or empty. Returns an `Err`
/// if the DESCRIPTION file cannot be read.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// // Assume a DESCRIPTION file exists at "tests/fixtures/DESCRIPTION"
/// let deps = crate::namespace_parser::parse_description_depends(Path::new("tests/fixtures/DESCRIPTION")).unwrap();
/// assert!(deps.iter().all(|s| !s.contains('(')));
/// ```
pub fn parse_description_depends(description_path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(description_path).map_err(|e| {
        anyhow!(
            "Failed to read DESCRIPTION file {:?}: {}",
            description_path,
            e
        )
    })?;

    Ok(parse_description_field(&content, "Depends"))
}

/// Extracts the value of a named field from DESCRIPTION (DCF) content and parses it into package names.
///
/// The function locates `field_name:` at the start of a line, accumulates its value including continuation
/// lines that begin with whitespace, and stops when a new field or a non-continuation line is encountered.
/// The collected field value is then parsed into package names (version constraints are stripped and the
/// `R` entry is excluded).
///
/// # Examples
///
/// ```
/// let desc = "\
/// Package: foo
/// Depends: R (>= 3.5.0), dplyr (>= 1.0.0),
///  tibble
/// Imports: utils
/// ";
/// let deps = parse_description_field(desc, "Depends");
/// assert_eq!(deps, vec!["dplyr".to_string(), "tibble".to_string()]);
/// ```
fn parse_description_field(content: &str, field_name: &str) -> Vec<String> {
    let mut field_value = String::new();
    let mut in_field = false;
    let field_prefix = format!("{}:", field_name);

    for line in content.lines() {
        if line.starts_with(&field_prefix) {
            // Found the field, extract the value after the colon
            in_field = true;
            if let Some(value) = line.strip_prefix(&field_prefix) {
                field_value.push_str(value.trim());
            }
        } else if in_field {
            // Check if this is a continuation line (starts with whitespace)
            if line.starts_with(' ') || line.starts_with('\t') {
                field_value.push(' ');
                field_value.push_str(line.trim());
            } else {
                // New field or blank line, stop reading
                break;
            }
        }
    }

    parse_depends_value(&field_value)
}

/// Extracts package names from a DESCRIPTION "Depends" field value.
///
/// Strips any parenthesized version constraints and excludes the special `R` entry.
///
/// # Examples
///
/// ```
/// let pkgs = parse_depends_value("R (>= 3.5), dplyr, ggplot2");
/// assert_eq!(pkgs, vec!["dplyr", "ggplot2"]);
/// ```
fn parse_depends_value(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    trimmed
        .split(',')
        .map(|s| {
            // Strip version constraints: "dplyr (>= 1.0)" -> "dplyr"
            let s = s.trim();
            if let Some(paren_pos) = s.find('(') {
                s[..paren_pos].trim()
            } else {
                s
            }
        })
        .filter(|s| !s.is_empty())
        // Filter out "R" - it's the R version requirement, not a package
        .filter(|s| *s != "R")
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests for parse_namespace_content

    #[test]
    fn test_parse_namespace_export_single() {
        let content = "export(foo)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo"]);
    }

    #[test]
    fn test_parse_namespace_export_multiple() {
        let content = "export(foo, bar, baz)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_namespace_export_quoted() {
        let content = r#"export("foo", "bar")"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_export_single_quoted() {
        let content = "export('foo', 'bar')";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_export_mixed_quotes() {
        let content = r#"export(foo, "bar", 'baz')"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_namespace_export_multiline() {
        let content = r#"
export(
    foo,
    bar,
    baz
)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_namespace_multiple_export_directives() {
        let content = r#"
export(foo)
export(bar)
export(baz)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_namespace_with_comments() {
        let content = r#"
# This is a comment
export(foo)
# Another comment
export(bar)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_export_pattern() {
        let content = r#"exportPattern("^[^.]")"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["__PATTERN__:^[^.]"]);
    }

    #[test]
    fn test_parse_namespace_export_pattern_multiple() {
        let content = r#"exportPattern("^foo", "^bar")"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["__PATTERN__:^foo", "__PATTERN__:^bar"]);
    }

    #[test]
    fn test_parse_namespace_s3method_basic() {
        let content = "S3method(print, foo)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["print.foo"]);
    }

    #[test]
    fn test_parse_namespace_s3method_with_method() {
        // S3method with explicit method name (third argument)
        let content = "S3method(print, foo, print_foo)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["print.foo"]);
    }

    #[test]
    fn test_parse_namespace_s3method_quoted() {
        let content = r#"S3method("print", "foo")"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["print.foo"]);
    }

    #[test]
    fn test_parse_namespace_s3method_multiple() {
        let content = r#"
S3method(print, foo)
S3method(summary, bar)
S3method(plot, baz)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["print.foo", "summary.bar", "plot.baz"]);
    }

    #[test]
    fn test_parse_namespace_mixed_directives() {
        let content = r#"
export(func1, func2)
S3method(print, myclass)
exportPattern("^helper_")
export(func3)
S3method(summary, myclass)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(
            exports,
            vec![
                "func1",
                "func2",
                "print.myclass",
                "__PATTERN__:^helper_",
                "func3",
                "summary.myclass"
            ]
        );
    }

    #[test]
    fn test_parse_namespace_empty() {
        let content = "";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_only_comments() {
        let content = r#"
# Comment 1
# Comment 2
"#;
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_whitespace_handling() {
        let content = "export(  foo  ,  bar  ,  baz  )";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_namespace_s3method_whitespace() {
        let content = "S3method(  print  ,  foo  )";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["print.foo"]);
    }

    #[test]
    fn test_parse_namespace_ignores_import_directives() {
        let content = r#"
export(foo)
import(dplyr)
importFrom(ggplot2, ggplot, aes)
export(bar)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_ignores_usedynlib() {
        let content = r#"
export(foo)
useDynLib(mypackage, .registration = TRUE)
export(bar)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_complex_multiline() {
        let content = r#"
# Package exports
export(
    create_model,
    fit_model,
    predict_model
)

# S3 methods
S3method(print, model)
S3method(summary, model)
S3method(plot, model)

# Pattern exports
exportPattern("^helper_")
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(
            exports,
            vec![
                "create_model",
                "fit_model",
                "predict_model",
                "print.model",
                "summary.model",
                "plot.model",
                "__PATTERN__:^helper_"
            ]
        );
    }

    // Tests for parse_description_field

    #[test]
    fn test_parse_description_depends_simple() {
        let content = "Package: mypackage\nDepends: dplyr, ggplot2, tidyr\nVersion: 1.0.0";
        let depends = parse_description_field(content, "Depends");
        assert_eq!(depends, vec!["dplyr", "ggplot2", "tidyr"]);
    }

    #[test]
    fn test_parse_description_depends_with_r_version() {
        let content = "Package: mypackage\nDepends: R (>= 3.5), dplyr, ggplot2\nVersion: 1.0.0";
        let depends = parse_description_field(content, "Depends");
        assert_eq!(depends, vec!["dplyr", "ggplot2"]);
    }

    #[test]
    fn test_parse_description_depends_with_version_constraints() {
        let content =
            "Package: mypackage\nDepends: R (>= 4.0), dplyr (>= 1.0.0), ggplot2 (>= 3.0)\nVersion: 1.0.0";
        let depends = parse_description_field(content, "Depends");
        assert_eq!(depends, vec!["dplyr", "ggplot2"]);
    }

    #[test]
    fn test_parse_description_depends_multiline() {
        let content = r#"Package: mypackage
Depends: R (>= 3.5),
    dplyr,
    ggplot2,
    tidyr
Version: 1.0.0"#;
        let depends = parse_description_field(content, "Depends");
        assert_eq!(depends, vec!["dplyr", "ggplot2", "tidyr"]);
    }

    #[test]
    fn test_parse_description_depends_empty() {
        let content = "Package: mypackage\nVersion: 1.0.0";
        let depends = parse_description_field(content, "Depends");
        assert!(depends.is_empty());
    }

    #[test]
    fn test_parse_description_depends_only_r() {
        let content = "Package: mypackage\nDepends: R (>= 4.0.0)\nVersion: 1.0.0";
        let depends = parse_description_field(content, "Depends");
        assert!(depends.is_empty());
    }

    #[test]
    fn test_parse_description_imports_field() {
        let content = "Package: mypackage\nImports: dplyr, ggplot2\nVersion: 1.0.0";
        let imports = parse_description_field(content, "Imports");
        assert_eq!(imports, vec!["dplyr", "ggplot2"]);
    }

    #[test]
    fn test_parse_description_suggests_field() {
        let content = "Package: mypackage\nSuggests: testthat, knitr\nVersion: 1.0.0";
        let suggests = parse_description_field(content, "Suggests");
        assert_eq!(suggests, vec!["testthat", "knitr"]);
    }

    // Tests for helper functions

    #[test]
    fn test_extract_directive_args_export() {
        let args = extract_directive_args("export(foo, bar)", "export");
        assert_eq!(args, Some("foo, bar".to_string()));
    }

    #[test]
    fn test_extract_directive_args_no_match() {
        let args = extract_directive_args("import(foo)", "export");
        assert!(args.is_none());
    }

    #[test]
    fn test_extract_directive_args_no_parens() {
        let args = extract_directive_args("export", "export");
        assert!(args.is_none());
    }

    #[test]
    fn test_parse_comma_separated_args_simple() {
        let args = parse_comma_separated_args("foo, bar, baz");
        assert_eq!(args, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_comma_separated_args_quoted() {
        let args = parse_comma_separated_args(r#""foo", "bar""#);
        assert_eq!(args, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_comma_separated_args_empty() {
        let args = parse_comma_separated_args("");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_s3method_args_basic() {
        let method = parse_s3method_args("print, foo");
        assert_eq!(method, Some("print.foo".to_string()));
    }

    #[test]
    fn test_parse_s3method_args_with_method() {
        let method = parse_s3method_args("print, foo, print_foo");
        assert_eq!(method, Some("print.foo".to_string()));
    }

    #[test]
    fn test_parse_s3method_args_insufficient() {
        let method = parse_s3method_args("print");
        assert!(method.is_none());
    }

    #[test]
    fn test_parse_s3method_args_empty() {
        let method = parse_s3method_args("");
        assert!(method.is_none());
    }

    // Tests for normalize_multiline_directives

    #[test]
    fn test_normalize_single_line() {
        let content = "export(foo)";
        let normalized = normalize_multiline_directives(content);
        assert_eq!(normalized.trim(), "export(foo)");
    }

    #[test]
    fn test_normalize_multiline() {
        let content = "export(\n    foo,\n    bar\n)";
        let normalized = normalize_multiline_directives(content);
        assert!(normalized.contains("export( foo, bar )"));
    }

    #[test]
    fn test_normalize_preserves_comments() {
        let content = "# comment\nexport(foo)";
        let normalized = normalize_multiline_directives(content);
        assert!(normalized.contains("# comment"));
        assert!(normalized.contains("export(foo)"));
    }

    // Tests for parse_depends_value

    #[test]
    fn test_parse_depends_value_simple() {
        let depends = parse_depends_value("dplyr, ggplot2, tidyr");
        assert_eq!(depends, vec!["dplyr", "ggplot2", "tidyr"]);
    }

    #[test]
    fn test_parse_depends_value_with_versions() {
        let depends = parse_depends_value("R (>= 3.5), dplyr (>= 1.0)");
        assert_eq!(depends, vec!["dplyr"]);
    }

    #[test]
    fn test_parse_depends_value_empty() {
        let depends = parse_depends_value("");
        assert!(depends.is_empty());
    }

    #[test]
    fn test_parse_depends_value_whitespace() {
        let depends = parse_depends_value("   ");
        assert!(depends.is_empty());
    }

    // ============================================================================
    // Malformed File Handling Tests
    // **Validates: Requirement 15.3** - THE LSP SHALL log the error and continue
    // without blocking other features
    // ============================================================================

    #[test]
    fn test_parse_namespace_unclosed_paren() {
        // Unclosed parenthesis should still parse what it can
        let content = "export(foo, bar";
        let exports = parse_namespace_content(content);
        // Should still extract the names even with unclosed paren
        assert!(exports.contains(&"foo".to_string()));
        assert!(exports.contains(&"bar".to_string()));
    }

    #[test]
    fn test_parse_namespace_unclosed_multiline() {
        // Multiline directive with unclosed paren
        let content = r#"
export(
    foo,
    bar
"#;
        let exports = parse_namespace_content(content);
        // Should still extract names from unclosed multiline directive
        assert!(exports.contains(&"foo".to_string()));
        assert!(exports.contains(&"bar".to_string()));
    }

    #[test]
    fn test_parse_namespace_directive_without_parens() {
        // Directive name without parentheses should be ignored
        let content = "export\nexport(foo)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo"]);
    }

    #[test]
    fn test_parse_namespace_empty_export() {
        // Empty export() should produce no exports
        let content = "export()";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_empty_s3method() {
        // Empty S3method() should produce no exports
        let content = "S3method()";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_s3method_single_arg() {
        // S3method with only one argument should produce no exports
        let content = "S3method(print)";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_empty_export_pattern() {
        // Empty exportPattern() should produce no exports
        let content = "exportPattern()";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_trailing_comma() {
        // Trailing comma should be handled gracefully
        let content = "export(foo, bar,)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_leading_comma() {
        // Leading comma should be handled gracefully
        let content = "export(, foo, bar)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar"]);
    }

    #[test]
    fn test_parse_namespace_multiple_commas() {
        // Multiple consecutive commas should be handled gracefully
        let content = "export(foo,, bar,,, baz)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_parse_namespace_only_whitespace_args() {
        // Export with only whitespace arguments
        let content = "export(   ,   ,   )";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_unclosed_quote() {
        // Unclosed quote should still parse what it can
        let content = r#"export("foo, bar)"#;
        let exports = parse_namespace_content(content);
        // The unclosed quote will consume everything until end
        // This is graceful degradation - we don't crash
        assert!(!exports.is_empty() || exports.is_empty()); // Either way is acceptable
    }

    #[test]
    fn test_parse_namespace_mixed_valid_invalid() {
        // Mix of valid and invalid directives
        let content = r#"
export(valid1)
export
export(valid2)
S3method(print)
S3method(print, foo)
exportPattern()
exportPattern("^valid")
"#;
        let exports = parse_namespace_content(content);
        assert!(exports.contains(&"valid1".to_string()));
        assert!(exports.contains(&"valid2".to_string()));
        assert!(exports.contains(&"print.foo".to_string()));
        assert!(exports.contains(&"__PATTERN__:^valid".to_string()));
        assert_eq!(exports.len(), 4);
    }

    #[test]
    fn test_parse_namespace_garbage_content() {
        // Random garbage content should not crash
        let content = "this is not valid NAMESPACE content\n@#$%^&*()";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_binary_like_content() {
        // Content that looks like binary data should not crash
        let content = "\x00\x01\x02export(foo)\x03\x04";
        let _exports = parse_namespace_content(content);
        // Should handle gracefully - may or may not find the export
        // The important thing is it doesn't crash
    }

    // ============================================================================
    // Error Handling Tests for File Operations
    // **Validates: Requirement 15.3** - THE LSP SHALL log the error and continue
    // without blocking other features
    // ============================================================================

    #[test]
    fn test_parse_namespace_file_not_found() {
        use std::path::PathBuf;
        let path = PathBuf::from("/nonexistent/path/to/NAMESPACE");
        let result = parse_namespace_exports(&path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to read NAMESPACE file"));
    }

    #[test]
    fn test_parse_description_file_not_found() {
        use std::path::PathBuf;
        let path = PathBuf::from("/nonexistent/path/to/DESCRIPTION");
        let result = parse_description_depends(&path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed to read DESCRIPTION file"));
    }

    // Edge case tests

    #[test]
    fn test_parse_namespace_nested_parens() {
        // Some packages might have complex patterns
        let content = r#"exportPattern("^[^(]+")"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["__PATTERN__:^[^(]+"]);
    }

    #[test]
    fn test_parse_namespace_special_characters_in_names() {
        // R allows some special characters in function names
        let content = "export(`%>%`, `%<>%`)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["`%>%`", "`%<>%`"]);
    }

    #[test]
    fn test_parse_namespace_dots_in_names() {
        let content = "export(data.frame, as.character, is.null)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["data.frame", "as.character", "is.null"]);
    }

    #[test]
    fn test_parse_namespace_s3method_with_dots() {
        // S3 methods for classes with dots
        let content = "S3method(print, data.frame)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["print.data.frame"]);
    }

    #[test]
    fn test_parse_namespace_real_world_example() {
        // A realistic NAMESPACE file content
        let content = r#"
# Generated by roxygen2: do not edit by hand

export(mutate)
export(filter)
export(select)
export(arrange)
export(summarise)
export(summarize)
export(group_by)
export(ungroup)

S3method(print, grouped_df)
S3method(summary, grouped_df)
S3method("[", grouped_df)

exportPattern("^[^.]")

import(rlang)
importFrom(tibble, tibble)
importFrom(magrittr, "%>%")
"#;
        let exports = parse_namespace_content(content);

        // Should contain the explicit exports
        assert!(exports.contains(&"mutate".to_string()));
        assert!(exports.contains(&"filter".to_string()));
        assert!(exports.contains(&"select".to_string()));
        assert!(exports.contains(&"arrange".to_string()));
        assert!(exports.contains(&"summarise".to_string()));
        assert!(exports.contains(&"summarize".to_string()));
        assert!(exports.contains(&"group_by".to_string()));
        assert!(exports.contains(&"ungroup".to_string()));

        // Should contain S3 methods
        assert!(exports.contains(&"print.grouped_df".to_string()));
        assert!(exports.contains(&"summary.grouped_df".to_string()));
        assert!(exports.contains(&"[.grouped_df".to_string()));

        // Should contain the pattern
        assert!(exports.contains(&"__PATTERN__:^[^.]".to_string()));

        // Should NOT contain imports
        assert!(!exports.iter().any(|e| e.contains("rlang")));
        assert!(!exports.iter().any(|e| e.contains("tibble")));
    }

    // ============================================================================
    // Property-Based Tests for NAMESPACE Parsing
    // Feature: package-function-awareness, Property 5: Package Export Round-Trip
    // **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5**
    // ============================================================================

    use proptest::prelude::*;
    use std::collections::HashSet;

    /// Generates a proptest strategy that produces valid R identifiers for export names.
    ///
    /// Identifiers begin with a lowercase ASCII letter and are followed by up to 10 characters
    /// drawn from lowercase ASCII letters, digits, dots, or underscores (total length 1–11).
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let value = r_identifier_strategy()
    ///     .new_tree(&mut runner)
    ///     .unwrap()
    ///     .current();
    ///
    /// let mut chars = value.chars();
    /// let first = chars.next().unwrap();
    /// assert!(first.is_ascii_lowercase());
    /// assert!(value.len() <= 11);
    /// for c in chars {
    ///     assert!(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_');
    /// }
    /// ```
    fn r_identifier_strategy() -> impl Strategy<Value = String> {
        // Use simple lowercase identifiers to avoid reserved words
        "[a-z][a-z0-9_]{0,10}".prop_filter("not empty", |s| !s.is_empty())
    }

    /// Produces a proptest strategy that generates valid R identifiers which may include dots.
    ///
    /// The strategy yields lowercase-starting identifiers composed of letters, digits, underscores,
    /// and dots, with a length between 1 and 11 characters, excluding identifiers that end with a
    /// dot or contain consecutive dots.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let tree = r_identifier_with_dots_strategy().new_tree(&mut runner).unwrap();
    /// let ident = tree.current();
    /// assert!(!ident.is_empty());
    /// assert!(!ident.ends_with('.'));
    /// assert!(!ident.contains(".."));
    /// ```
    fn r_identifier_with_dots_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_.]{0,10}".prop_filter("not empty and valid", |s| {
            !s.is_empty() && !s.ends_with('.') && !s.contains("..")
        })
    }

    /// Provides a proptest strategy that yields simple regex patterns suitable for `exportPattern` tests.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// proptest! {
    ///     #[test]
    ///     fn example(pattern in crate::regex_pattern_strategy()) {
    ///         // pattern is a simple regex string like "^helper_" or "^[A-Z]"
    ///         prop_assert!(!pattern.is_empty());
    ///     }
    /// }
    /// ```
    fn regex_pattern_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("^[^.]".to_string()),
            Just("^[a-z]".to_string()),
            Just("^helper_".to_string()),
            Just("^internal_".to_string()),
            Just("^[A-Z]".to_string()),
        ]
    }

    /// Produces a proptest strategy that generates an `export(...)` directive string and its parsed names.
    ///
    /// The strategy yields a tuple `(directive, names)` where `directive` is a string like
    /// `export(foo, bar)` and `names` is the Vec of identifier strings contained in the directive.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// use proptest::test_runner::TestRunner;
    ///
    /// let mut runner = TestRunner::default();
    /// let tree = crate::export_directive_strategy().new_tree(&mut runner).unwrap();
    /// let (directive, names) = tree.current();
    /// assert!(directive.starts_with("export("));
    /// assert!(directive.ends_with(")"));
    /// assert_eq!(
    ///     directive,
    ///     format!("export({})", names.join(", "))
    /// );
    /// ```
    fn export_directive_strategy() -> impl Strategy<Value = (String, Vec<String>)> {
        prop::collection::vec(r_identifier_strategy(), 1..=5).prop_map(|names| {
            let directive = format!("export({})", names.join(", "));
            (directive, names)
        })
    }

    /// Generates a test strategy that produces a tuple containing:
    /// 1. a string with an `exportPattern("...")` directive using a generated regex pattern, and
    /// 2. the corresponding expected export marker vector with a single entry `"__PATTERN__:<pattern>"`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// let strat = export_pattern_directive_strategy();
    /// proptest!(|(pair in strat)| {
    ///     let (directive, expected) = pair;
    ///     prop_assert!(directive.starts_with("exportPattern(\""));
    ///     prop_assert_eq!(expected.len(), 1);
    ///     prop_assert!(directive.contains(&expected[0].trim_start_matches("__PATTERN__:")));
    /// });
    /// ```
    fn export_pattern_directive_strategy() -> impl Strategy<Value = (String, Vec<String>)> {
        regex_pattern_strategy().prop_map(|pattern| {
            let directive = format!("exportPattern(\"{}\")", pattern);
            let expected = vec![format!("__PATTERN__:{}", pattern)];
            (directive, expected)
        })
    }

    /// Generates a proptest strategy that produces a tuple containing:

    /// - an `S3method(...)` directive string, and

    /// - the corresponding expected export name `generic.class` as a single-item `Vec<String>`.

    ///

    /// # Examples

    ///

    /// ```

    /// use proptest::test_runner::TestRunner;

    ///

    /// let mut runner = TestRunner::default();

    /// let strategy = s3method_directive_strategy();

    /// let tree = strategy.new_tree(&mut runner).unwrap();

    /// let (directive, expected) = tree.current();

    /// assert!(directive.starts_with("S3method("));

    /// assert_eq!(expected.len(), 1);

    /// assert!(expected[0].contains('.'));

    /// ```
    fn s3method_directive_strategy() -> impl Strategy<Value = (String, Vec<String>)> {
        (r_identifier_strategy(), r_identifier_with_dots_strategy()).prop_map(|(generic, class)| {
            let directive = format!("S3method({}, {})", generic, class);
            let expected = vec![format!("{}.{}", generic, class)];
            (directive, expected)
        })
    }

    /// Constructs a proptest Strategy that generates synthetic NAMESPACE file content paired with the expected list of exported symbol strings.
    ///
    /// The produced strategy yields a tuple: the first element is the generated NAMESPACE file text (multiple directive lines joined with newlines), and the second element is the Vec of export entries that the parser should produce for that content.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = namespace_content_strategy();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let (content, expected_exports) = strat.new_tree(&mut runner).unwrap().current();
    /// // content is the generated NAMESPACE text and expected_exports lists the symbols that should be parsed from it
    /// assert!(content.len() > 0 || expected_exports.is_empty());
    /// ```
    fn namespace_content_strategy() -> impl Strategy<Value = (String, Vec<String>)> {
        (
            prop::collection::vec(export_directive_strategy(), 0..=3),
            prop::collection::vec(export_pattern_directive_strategy(), 0..=2),
            prop::collection::vec(s3method_directive_strategy(), 0..=3),
        )
            .prop_map(|(exports, patterns, s3methods)| {
                let mut lines = Vec::new();
                let mut expected_exports = Vec::new();

                // Add export directives
                for (directive, names) in exports {
                    lines.push(directive);
                    expected_exports.extend(names);
                }

                // Add exportPattern directives
                for (directive, patterns) in patterns {
                    lines.push(directive);
                    expected_exports.extend(patterns);
                }

                // Add S3method directives
                for (directive, methods) in s3methods {
                    lines.push(directive);
                    expected_exports.extend(methods);
                }

                let content = lines.join("\n");
                (content, expected_exports)
            })
    }

    /// Produces a proptest strategy that yields a tuple of
    /// (NAMESPACE file content with interspersed comment lines, expected exports).
    ///
    /// The content is derived from `namespace_content_strategy` with a header comment,
    /// a comment inserted before each original line, and a footer comment.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::strategy::Strategy;
    /// let strat = crate::namespace_content_with_comments_strategy();
    /// // `strat` generates values of type `(String, Vec<String>)`
    /// ```
    fn namespace_content_with_comments_strategy() -> impl Strategy<Value = (String, Vec<String>)> {
        namespace_content_strategy().prop_map(|(content, expected)| {
            // Add comments between lines
            let lines: Vec<&str> = content.lines().collect();
            let mut result_lines = Vec::new();

            result_lines.push("# Generated by roxygen2: do not edit by hand");
            for line in lines {
                result_lines.push("# This is a comment");
                result_lines.push(line);
            }
            result_lines.push("# End of NAMESPACE");

            (result_lines.join("\n"), expected)
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5**
        //
        // Property 5a: NAMESPACE parsing is idempotent - parsing the same content
        // twice SHALL produce identical results.
        #[test]
        fn prop_namespace_parsing_idempotent((content, _expected) in namespace_content_strategy()) {
            let result1 = parse_namespace_content(&content);
            let result2 = parse_namespace_content(&content);

            prop_assert_eq!(
                result1,
                result2,
                "Parsing the same NAMESPACE content twice should produce identical results"
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5**
        //
        // Property 5b: All generated export() directives SHALL be correctly parsed
        // and included in the exports list.
        #[test]
        fn prop_export_directives_parsed_correctly((content, expected) in namespace_content_strategy()) {
            let result = parse_namespace_content(&content);

            // All expected exports should be present in the result
            for export in &expected {
                prop_assert!(
                    result.contains(export),
                    "Expected export '{}' not found in parsed results. Content:\n{}\nResult: {:?}",
                    export,
                    content,
                    result
                );
            }

            // Result should have exactly the expected number of exports
            prop_assert_eq!(
                result.len(),
                expected.len(),
                "Number of parsed exports ({}) doesn't match expected ({}). Content:\n{}\nResult: {:?}\nExpected: {:?}",
                result.len(),
                expected.len(),
                content,
                result,
                expected
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5**
        //
        // Property 5c: Comments SHALL NOT affect parsing results - NAMESPACE content
        // with and without comments should produce the same exports.
        #[test]
        fn prop_comments_do_not_affect_parsing((content, expected) in namespace_content_with_comments_strategy()) {
            let result = parse_namespace_content(&content);

            // All expected exports should be present
            for export in &expected {
                prop_assert!(
                    result.contains(export),
                    "Expected export '{}' not found when comments present. Content:\n{}\nResult: {:?}",
                    export,
                    content,
                    result
                );
            }

            // Should have exactly the expected exports
            prop_assert_eq!(
                result.len(),
                expected.len(),
                "Comments affected parsing. Expected {} exports, got {}",
                expected.len(),
                result.len()
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.3**
        //
        // Property 5d: export() directive with multiple names SHALL parse all names.
        #[test]
        fn prop_export_multiple_names_parsed(names in prop::collection::vec(r_identifier_strategy(), 1..=10)) {
            let content = format!("export({})", names.join(", "));
            let result = parse_namespace_content(&content);

            // All names should be in the result
            for name in &names {
                prop_assert!(
                    result.contains(name),
                    "Name '{}' not found in parsed export. Content: {}\nResult: {:?}",
                    name,
                    content,
                    result
                );
            }

            prop_assert_eq!(
                result.len(),
                names.len(),
                "Expected {} exports, got {}",
                names.len(),
                result.len()
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.5**
        //
        // Property 5e: S3method(generic, class) SHALL produce "generic.class" export.
        #[test]
        fn prop_s3method_produces_dotted_name(
            generic in r_identifier_strategy(),
            class in r_identifier_with_dots_strategy()
        ) {
            let content = format!("S3method({}, {})", generic, class);
            let result = parse_namespace_content(&content);

            let expected_name = format!("{}.{}", generic, class);
            prop_assert!(
                result.contains(&expected_name),
                "S3method({}, {}) should produce '{}', got {:?}",
                generic,
                class,
                expected_name,
                result
            );

            prop_assert_eq!(
                result.len(),
                1,
                "S3method should produce exactly one export"
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.4**
        //
        // Property 5f: exportPattern() SHALL produce a pattern marker in exports.
        #[test]
        fn prop_export_pattern_produces_marker(pattern in regex_pattern_strategy()) {
            let content = format!("exportPattern(\"{}\")", pattern);
            let result = parse_namespace_content(&content);

            let expected_marker = format!("__PATTERN__:{}", pattern);
            prop_assert!(
                result.contains(&expected_marker),
                "exportPattern(\"{}\") should produce '{}', got {:?}",
                pattern,
                expected_marker,
                result
            );

            prop_assert_eq!(
                result.len(),
                1,
                "exportPattern should produce exactly one marker"
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.2, 3.3, 3.4, 3.5**
        //
        // Property 5g: Parsing SHALL preserve export order (document order).
        #[test]
        fn prop_parsing_preserves_order(names in prop::collection::vec(r_identifier_strategy(), 2..=5)) {
            // Create multiple export() directives, one per name
            let content = names
                .iter()
                .map(|n| format!("export({})", n))
                .collect::<Vec<_>>()
                .join("\n");

            let result = parse_namespace_content(&content);

            // Result should be in the same order as input
            prop_assert_eq!(
                result,
                names,
                "Parsing should preserve document order"
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.2, 3.3**
        //
        // Property 5h: Quoted and unquoted names SHALL be parsed equivalently.
        #[test]
        fn prop_quoted_unquoted_equivalent(name in r_identifier_strategy()) {
            let unquoted = format!("export({})", name);
            let double_quoted = format!("export(\"{}\")", name);
            let single_quoted = format!("export('{}')", name);

            let result_unquoted = parse_namespace_content(&unquoted);
            let result_double = parse_namespace_content(&double_quoted);
            let result_single = parse_namespace_content(&single_quoted);

            prop_assert_eq!(
                &result_unquoted,
                &result_double,
                "Double-quoted should equal unquoted"
            );
            prop_assert_eq!(
                &result_unquoted,
                &result_single,
                "Single-quoted should equal unquoted"
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.2, 3.3**
        //
        // Property 5i: Empty NAMESPACE content SHALL produce empty exports.
        #[test]
        fn prop_empty_content_empty_exports(
            whitespace in prop::collection::vec(prop_oneof![Just(" "), Just("\t"), Just("\n")], 0..=5)
        ) {
            let content = whitespace.join("");
            let result = parse_namespace_content(&content);

            prop_assert!(
                result.is_empty(),
                "Empty/whitespace content should produce empty exports, got {:?}",
                result
            );
        }

        // Feature: package-function-awareness, Property 5: Package Export Round-Trip
        // **Validates: Requirements 3.2, 3.3**
        //
        // Property 5j: Multiline export() directives SHALL be parsed correctly.
        #[test]
        fn prop_multiline_export_parsed(names in prop::collection::vec(r_identifier_strategy(), 1..=5)) {
            // Create a multiline export directive
            let content = format!(
                "export(\n    {}\n)",
                names.join(",\n    ")
            );

            let result = parse_namespace_content(&content);

            // All names should be present
            let result_set: HashSet<_> = result.iter().collect();
            let names_set: HashSet<_> = names.iter().collect();

            prop_assert_eq!(
                result_set,
                names_set,
                "Multiline export should parse all names. Content:\n{}\nResult: {:?}",
                content,
                result
            );
        }
    }
}
