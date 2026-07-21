// namespace_parser.rs - NAMESPACE and DESCRIPTION file parsing for package exports
//
// This module provides fallback parsing for R package metadata when R subprocess
// is unavailable. It parses NAMESPACE files to extract exported symbols and
// DESCRIPTION files to extract package dependencies.
//
// Requirement 3.2: IF R subprocess is unavailable, THE Package_Resolver SHALL
// fall back to parsing the package's NAMESPACE file directly

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

static INDEX_EXPORTS_CACHE: OnceLock<DashMap<PathBuf, Vec<String>>> = OnceLock::new();

fn index_exports_cache() -> &'static DashMap<PathBuf, Vec<String>> {
    INDEX_EXPORTS_CACHE.get_or_init(DashMap::new)
}

/// Extracts exported symbol names from an R package NAMESPACE file.
///
/// Recognizes `export(name[, ...])`, `exportPattern("pattern")`,
/// `exportMethods(name[, ...])`, `exportClasses(name[, ...])`,
/// `exportClassPattern("pattern")` and `S3method(generic, class[, method])`.
/// Comments and blank lines are ignored; multiline directives are supported.
/// `exportPattern` and `exportClassPattern` entries are preserved as markers in
/// the form `__PATTERN__:<pattern>`. `exportMethods` (S4 generics/methods) and
/// `exportClasses` (S4 class names) are returned as plain names, matching
/// `getNamespaceExports()`. S3 methods are returned as `generic.class`.
/// Whitespace is trimmed and empty names are omitted.
///
/// # Returns
///
/// A `Vec<String>` containing exported symbol names in document order. Pattern exports appear as
/// `__PATTERN__:<pattern>` and S3 methods as `generic.class`.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use raven::namespace_parser::parse_namespace_exports;
/// let exports = parse_namespace_exports(Path::new("NAMESPACE")).unwrap();
/// // `exports` may contain entries like "foo", "__PATTERN__:^bar.*$", or "print.myclass"
/// ```
pub fn parse_namespace_exports(namespace_path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(namespace_path)
        .map_err(|e| anyhow!("Failed to read NAMESPACE file {:?}: {}", namespace_path, e))?;

    Ok(parse_namespace_content(&content))
}

/// Split a package's NAMESPACE into its explicit exports and whether it uses any
/// pattern export (`exportPattern` / `exportClassPattern`).
///
/// "Explicit" exports are the `export()` / `S3method()` / `exportClasses()` /
/// `exportMethods()` names, with the internal `__PATTERN__:` markers removed.
/// The bool is `true` when any pattern marker was present (those need INDEX or R
/// to expand). A missing or unreadable NAMESPACE yields `(vec![], true)` — the
/// same "treat as pattern, fall back to INDEX/R" stance the async loader takes —
/// so callers never mistake "couldn't read" for "exports nothing".
///
/// Single source of truth for the explicit-vs-pattern split, shared by
/// [`crate::package_library::PackageLibrary::parse_package_static`] (async load)
/// and `PackageLibrary::get_exports_sync` (sync `pkg::` completion).
pub fn parse_namespace_explicit_exports(namespace_path: &Path) -> (Vec<String>, bool) {
    match parse_namespace_exports(namespace_path) {
        Ok(raw) => {
            let has_pattern = raw.iter().any(|e| e.starts_with("__PATTERN__:"));
            let explicit = raw
                .into_iter()
                .filter(|e| !e.starts_with("__PATTERN__:"))
                .collect();
            (explicit, has_pattern)
        }
        Err(_) => (Vec::new(), true),
    }
}

/// Extract explicit names that a successful package attachment places on the
/// search path.
///
/// Unlike [`parse_namespace_explicit_exports`], this diagnostic-only view does
/// not include `S3method(...)` registrations: registering a method does not by
/// itself export or attach the method binding. Pattern exports are also omitted
/// because this static parser cannot expand them. Missing or unreadable files
/// therefore fail closed to no positive evidence.
pub(crate) fn parse_namespace_attachment_visible_explicit_exports(
    namespace_path: &Path,
) -> Vec<String> {
    let Ok(content) = fs::read_to_string(namespace_path) else {
        return Vec::new();
    };
    let normalized = normalize_multiline_directives(&content);
    let mut exports = Vec::new();

    for line in normalized.lines().map(str::trim) {
        let args = extract_directive_args(line, "export")
            .or_else(|| extract_directive_args(line, "exportClasses"))
            .or_else(|| extract_directive_args(line, "exportMethods"));
        let Some(args) = args else {
            continue;
        };
        exports.extend(
            parse_comma_separated_args(&args)
                .into_iter()
                .filter(|name| !name.is_empty()),
        );
    }

    exports
}

/// Parse NAMESPACE file content to extract exported symbols and pattern markers.
///
/// This function scans normalized NAMESPACE directives and collects:
///
/// - plain exported names from `export(...)` directives,
/// - S4 generic/method names from `exportMethods(...)` and S4 class names from
///   `exportClasses(...)` as plain names (issue #474 — these appear in
///   `getNamespaceExports()` but were previously dropped),
/// - S3 methods from `S3method(generic, class[, method])` formatted as `generic.class`,
/// - export patterns from `exportPattern(...)` and `exportClassPattern(...)`
///   stored as markers `__PATTERN__:<pattern>`.
///
/// Comments and blank lines are ignored; whitespace is trimmed and empty names are filtered out.
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
        // Handle exportClassPattern("pattern") — S4 analogue of exportPattern,
        // matching exported S4 class names by regex. Like exportPattern we can't
        // expand it without R/INDEX, so store it as a marker (issue #474).
        // Checked before `exportClasses` is irrelevant (distinct prefixes), but
        // it must be checked before any `exportClass`-style prefix would match.
        else if let Some(args) = extract_directive_args(line, "exportClassPattern") {
            for pattern in parse_comma_separated_args(&args) {
                if !pattern.is_empty() {
                    exports.push(format!("__PATTERN__:{}", pattern));
                }
            }
        }
        // Handle exportClasses(Class1, Class2, ...) — exported S4 class names.
        // `getNamespaceExports()` includes these, so the static parse must too
        // (issue #474). Names are plain identifiers (some are also generator
        // functions, e.g. sp's `CRS`).
        else if let Some(args) = extract_directive_args(line, "exportClasses") {
            for name in parse_comma_separated_args(&args) {
                if !name.is_empty() {
                    exports.push(name);
                }
            }
        }
        // Handle exportMethods(generic1, generic2, ...) — exported S4 generics
        // /methods. `getNamespaceExports()` returns these as the generic names
        // (issue #474: sp::spTransform, maptools::spRbind, sp::spChFIDs).
        else if let Some(args) = extract_directive_args(line, "exportMethods") {
            for name in parse_comma_separated_args(&args) {
                if !name.is_empty() {
                    exports.push(name);
                }
            }
        }
        // Handle S3method(generic, class) and S3method(generic, class, method)
        // Requirement 3.5: WHEN a NAMESPACE file contains `S3method(generic, class)`,
        // THE Package_Resolver SHALL include the S3 method in exports
        else if let Some(args) = extract_directive_args(line, "S3method")
            && let Some(method_name) = parse_s3method_args(&args)
        {
            exports.push(method_name);
        }
    }

    exports
}

/// Collapse NAMESPACE directives that span multiple lines into single logical lines.
///
/// Drops comments outside quoted strings before joining, so comments inside a
/// parenthesized directive do not become part of the next argument token. If a
/// directive's parentheses never close, the accumulated line is emitted as-is at
/// the end.
fn normalize_multiline_directives(content: &str) -> String {
    let mut result = String::new();
    let mut current_line = String::new();
    let mut paren_depth: i32 = 0;

    for line in content.lines() {
        let trimmed = strip_comment_outside_quotes(line).trim();

        // Skip blank and comment-only lines. Inside a directive, comments are
        // not arguments; keeping them would glue the comment text to the next
        // export token (e.g. `# Listing` + `"grid.ls"`).
        if trimmed.is_empty() {
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

/// Strip an R-style comment from a NAMESPACE line, preserving `#` characters
/// that occur inside single- or double-quoted strings.
fn strip_comment_outside_quotes(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }

        match ch {
            '"' | '\'' => quote = Some(ch),
            '#' => return &line[..idx],
            _ => {}
        }
    }

    line
}
/// Extracts the argument text for a directive like `export(arg1, arg2)`.
///
/// Returns the string between the directive's outer parentheses if the given line
/// starts with the directive name followed immediately by `(`; returns `None`
/// if the line does not start with the directive or lacks the opening `(`.
/// If a matching closing parenthesis cannot be found, returns the remainder of
/// the line after the opening `(` with any trailing `)` characters trimmed.
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
/// ```no_run
/// use std::path::Path;
/// use raven::namespace_parser::parse_description_depends;
/// let deps = parse_description_depends(Path::new("tests/fixtures/DESCRIPTION")).unwrap();
/// assert!(deps.iter().all(|s: &String| !s.contains('(')));
/// ```
pub fn parse_description_depends(description_path: &Path) -> Result<Vec<String>> {
    let content = read_description(description_path)?;
    Ok(parse_description_field(&content, "Depends"))
}

/// Public wrapper around `parse_description_field` for use from other modules
/// that need to inspect dependency lists (e.g., the package-mode derivation
/// querying `Suggests:` / `Imports:` / `Depends:` for implicit attachment in
/// `tests/testthat/`). Strips version constraints and the special `R` entry.
pub fn parse_description_field_pub(content: &str, field_name: &str) -> Vec<String> {
    parse_description_field(content, field_name)
}

/// Extracts the value of a named field from DESCRIPTION (DCF) content and parses it into package names.
///
/// Reads the raw field value via the shared DCF parser
/// [`crate::package_namespace::parse_dcf_field_pub`] and then parses it into
/// package names (version constraints are stripped and the `R` entry is excluded).
fn parse_description_field(content: &str, field_name: &str) -> Vec<String> {
    match crate::package_namespace::parse_dcf_field_pub(content, field_name) {
        Some(value) => parse_depends_value(&value),
        None => Vec::new(),
    }
}

/// Free-text metadata fields from a package's DESCRIPTION (DCF) file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageDescription {
    /// The one-line `Title:` field.
    pub title: Option<String>,
    /// The prose `Description:` field (continuation lines folded to one line).
    pub description: Option<String>,
}

/// Parses the `Title` and `Description` free-text fields from DESCRIPTION content.
///
/// Unlike [`parse_description_depends`], these are prose fields, so each value is
/// returned verbatim (via the shared [`crate::package_namespace::parse_dcf_field_pub`],
/// which folds continuation lines and skips DCF blank-paragraph `.` separators)
/// without the comma-splitting / version-stripping / `R`-filtering applied to
/// dependency fields. Missing or blank fields yield `None`.
pub fn parse_description_metadata_str(content: &str) -> PackageDescription {
    PackageDescription {
        title: crate::package_namespace::parse_dcf_field_pub(content, "Title"),
        description: crate::package_namespace::parse_dcf_field_pub(content, "Description"),
    }
}

/// Reads and parses the `Title`/`Description` fields from a DESCRIPTION file.
///
/// Sibling to [`parse_description_depends`]; used by hover to attribute the
/// package side of `pkg::name` (issue #382 step 1). Returns an `Err` only if the
/// file cannot be read — a present-but-fieldless DESCRIPTION yields a
/// `PackageDescription` with `None` fields.
pub fn parse_description_metadata(description_path: &Path) -> Result<PackageDescription> {
    Ok(parse_description_metadata_str(&read_description(
        description_path,
    )?))
}

/// Reads a DESCRIPTION (DCF) file to a string, mapping any I/O error to a
/// consistent message. Shared by [`parse_description_depends`] and
/// [`parse_description_metadata`].
fn read_description(description_path: &Path) -> Result<String> {
    fs::read_to_string(description_path).map_err(|e| {
        anyhow!(
            "Failed to read DESCRIPTION file {:?}: {}",
            description_path,
            e
        )
    })
}

/// Extracts package names from a DESCRIPTION "Depends" field value.
///
/// Strips any parenthesized version constraints and excludes the special `R` entry.
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

/// Extracts documented export names from an R package INDEX file.
///
/// INDEX files list documented functions with their descriptions. The format is:
/// ```text
/// function_name           Description text that may span
///                         multiple lines (continuation lines
///                         start with whitespace)
/// another_function        Another description
/// ```
///
/// This function extracts only the function names (first column), ignoring descriptions.
/// This is used as a fallback for packages that use `exportPattern()` when the R
/// subprocess is unavailable.
///
/// Results are cached per `package_dir` to avoid repeated filesystem reads.
///
/// # Arguments
/// * `package_dir` - Path to the installed package directory
///
/// # Returns
/// A `Vec<String>` containing documented function names, or an error if the file
/// cannot be read.
///
/// # Examples
/// ```rust,no_run
/// use std::path::Path;
/// use raven::namespace_parser::parse_index_exports;
/// # async fn demo() -> anyhow::Result<()> {
/// let exports = parse_index_exports(Path::new("/usr/lib/R/library/dplyr")).await?;
/// assert!(exports.contains(&"mutate".to_string()));
/// # Ok(())
/// # }
/// ```
pub async fn parse_index_exports(package_dir: &Path) -> Result<Vec<String>> {
    let cache_key = package_dir.to_path_buf();
    if let Some(cached) = index_exports_cache().get(&cache_key) {
        return Ok(cached.value().clone());
    }

    let index_path = package_dir.join("INDEX");

    // Basic validation: `package_dir` should not contain path-traversal components.
    // (Defense in depth: package names ultimately come from user code.)
    if package_dir
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(anyhow!(
            "Suspicious package_dir {:?} when reading INDEX file {:?}",
            package_dir,
            index_path
        ));
    }

    // Offload filesystem I/O from the LSP request executor.
    let content = tokio::task::spawn_blocking({
        let index_path = index_path.clone();
        let package_dir = cache_key.clone();
        move || {
            if !package_dir.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("package_dir is not a directory: {:?}", package_dir),
                ));
            }
            fs::read_to_string(&index_path)
        }
    })
    .await
    .map_err(|e| anyhow!("Task join error reading INDEX file {:?}: {e}", index_path))?
    .map_err(|e| anyhow!("Failed to read INDEX file {:?}: {e}", index_path))?;

    let exports = parse_index_content(&content);
    index_exports_cache().insert(cache_key, exports.clone());
    Ok(exports)
}

/// Return the object name for a `data/` filename if it matches one of R's
/// recognized data file shapes; otherwise `None`.
///
/// Per `?data` (R 4.x), the data-loading mechanism accepts:
///   - `name.rda` / `name.RData` — serialized R objects.
///   - `name.R` / `name.r` — R script evaluated for side-effects.
///   - `name.csv` / `name.tab` / `name.txt`, optionally wrapped as
///     `name.<inner>.gz` / `.bz2` / `.xz` (the outer compression layer only
///     applies to the tabular text formats).
///
/// `.rda.gz` / `.R.gz` etc. are intentionally rejected — R does not load them.
fn data_file_stem<'a>(
    file_name: &'a str,
    serialized_or_script_exts: &[&str],
    tabular_exts: &[&str],
    compression_exts: &[&str],
) -> Option<&'a str> {
    let (stem_outer, ext_outer) = file_name.rsplit_once('.')?;
    let ext_outer_lc = ext_outer.to_ascii_lowercase();

    if serialized_or_script_exts.contains(&ext_outer_lc.as_str()) {
        return Some(stem_outer);
    }
    if tabular_exts.contains(&ext_outer_lc.as_str()) {
        return Some(stem_outer);
    }

    // Optional compression layer — only valid over tabular text formats.
    if compression_exts.contains(&ext_outer_lc.as_str()) {
        let (stem_inner, ext_inner) = stem_outer.rsplit_once('.')?;
        let ext_inner_lc = ext_inner.to_ascii_lowercase();
        if tabular_exts.contains(&ext_inner_lc.as_str()) {
            return Some(stem_inner);
        }
    }

    None
}

/// Collect "data symbols" from an installed R package's `data/` directory.
///
/// Some packages (notably base R's `datasets`) ship data objects that become
/// available when the package is attached, but are NOT listed in NAMESPACE
/// `export(...)` lines and do NOT appear in `getNamespaceExports()`. These
/// symbols therefore need separate discovery if they are to be recognized as
/// in-scope identifiers.
///
/// Strategy (combines static sources for best coverage):
/// 1. `data/datalist` — text file with one entry per line. Entries may be
///    `name` or `name: subname1 subname2`. Generated by `tools::add_datalist()`
///    when a package is installed.
/// 2. Individual files in `data/` — strips known data extensions (`.rda`,
///    `.RData`, `.rdata`, `.csv`, `.tab`, `.txt`, `.R`, `.r`) to recover
///    object names from filenames.
///
/// Lazy-loaded packages ship `Rdata.rdb` / `Rdata.rdx` / `Rdata.rds` instead
/// of one file per object; for those, the caller should additionally consult
/// the package's `INDEX` file via [`parse_index_exports`], which enumerates
/// documented data topics.
///
/// Returns an empty vec if the package has no `data/` directory or if any
/// I/O step fails — this helper is best-effort and never errors.
pub async fn parse_data_symbols(pkg_dir: &Path) -> Vec<String> {
    use std::collections::HashSet;

    // Defense in depth: reject `..` segments in `pkg_dir` so a malformed
    // library entry can't make us walk above its own tree. Mirrors the same
    // guard in `parse_index_exports`.
    if pkg_dir
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Vec::new();
    }

    let data_dir = pkg_dir.join("data");
    let mut symbols: HashSet<String> = HashSet::new();

    // Run filesystem checks off the LSP request executor.
    let collected = tokio::task::spawn_blocking({
        let data_dir = data_dir.clone();
        move || -> Vec<String> {
            let mut found: Vec<String> = Vec::new();

            // Require a real directory, not a symlink. Symlinks would let a
            // malformed installed-package tree (or a path under attacker
            // control) redirect us at arbitrary filesystem locations.
            let data_meta = match fs::symlink_metadata(&data_dir) {
                Ok(m) => m,
                Err(_) => return found,
            };
            if !data_meta.is_dir() {
                return found;
            }

            // 1. data/datalist (text file). Also require a regular file (not
            // a symlink) before reading, for the same reason.
            let datalist_path = data_dir.join("datalist");
            let datalist_ok = fs::symlink_metadata(&datalist_path)
                .map(|m| m.is_file())
                .unwrap_or(false);
            if datalist_ok && let Ok(content) = fs::read_to_string(&datalist_path) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((primary, rest)) = line.split_once(':') {
                        let primary = primary.trim();
                        if is_valid_r_identifier(primary) {
                            found.push(primary.to_string());
                        }
                        for sub in rest.split_whitespace() {
                            if is_valid_r_identifier(sub) {
                                found.push(sub.to_string());
                            }
                        }
                    } else if is_valid_r_identifier(line) {
                        found.push(line.to_string());
                    }
                }
            }

            // 2. Individual data files in data/
            // Skip lazy-load infrastructure and the datalist file itself.
            const SKIP_FILES: &[&str] = &["Rdata.rdb", "Rdata.rdx", "Rdata.rds", "datalist"];
            // Per `?data`, R recognizes these data-file extensions:
            //   - `.rda` / `.RData` — serialized R objects (already compressed
            //     internally; not externally wrapped).
            //   - `.R` / `.r` — R scripts evaluated for side-effects.
            //   - `.csv` / `.tab` / `.txt` — tabular text formats, optionally
            //     wrapped in `.gz`/`.bz2`/`.xz`.
            const SERIALIZED_OR_SCRIPT_EXTS: &[&str] = &["rda", "rdata", "r"];
            const TABULAR_EXTS: &[&str] = &["csv", "tab", "txt"];
            const COMPRESSION_EXTENSIONS: &[&str] = &["gz", "bz2", "xz"];

            let entries = match fs::read_dir(&data_dir) {
                Ok(e) => e,
                Err(_) => return found,
            };

            for entry in entries.flatten() {
                // Skip symlinks: regular files only. `DirEntry::file_type()`
                // does not traverse symlinks, so a symlinked entry resolves
                // to false here even if the link target is a regular file.
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }

                let path = entry.path();
                let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };

                if SKIP_FILES.contains(&file_name) {
                    continue;
                }

                let Some(stem) = data_file_stem(
                    file_name,
                    SERIALIZED_OR_SCRIPT_EXTS,
                    TABULAR_EXTS,
                    COMPRESSION_EXTENSIONS,
                ) else {
                    continue;
                };

                if is_valid_r_identifier(stem) {
                    found.push(stem.to_string());
                }
            }

            found
        }
    })
    .await
    .unwrap_or_default();

    for name in collected {
        symbols.insert(name);
    }

    let mut out: Vec<String> = symbols.into_iter().collect();
    out.sort();
    out
}

/// Parse INDEX file content to extract function names.
///
/// INDEX format: Lines starting with a non-whitespace character begin a new entry.
/// The function name is the first word on such lines. Continuation lines (starting
/// with whitespace) are ignored as they contain description text.
fn parse_index_content(content: &str) -> Vec<String> {
    let mut exports = Vec::new();

    for line in content.lines() {
        // Skip empty lines
        if line.is_empty() {
            continue;
        }

        // Continuation lines start with whitespace - skip them
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }

        // Extract the function name (first word before whitespace)
        // INDEX format: "function_name    Description text..."
        let name = line.split_whitespace().next();
        if let Some(name) = name {
            // Validate it looks like a valid R identifier
            if !name.is_empty() && is_valid_r_identifier(name) {
                exports.push(name.to_string());
            }
        }
    }

    exports
}

/// Check if a string looks like a valid R identifier.
///
/// Valid R identifiers:
/// - Start with a letter or dot (if dot, second char must be a letter or not exist)
/// - Contain letters, digits, dots, and underscores
/// - May be backtick-quoted (e.g., `%>%`)
fn is_valid_r_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    // Backtick-quoted identifiers are valid, but must not be empty.
    if s.starts_with('`') && s.ends_with('`') && s.len() > 2 {
        return true;
    }

    let mut chars = s.chars();
    let first = chars.next().unwrap();

    // Must start with letter or dot
    if !first.is_ascii_alphabetic() && first != '.' {
        return false;
    }

    // If starts with dot, second char must be letter (not digit)
    if first == '.'
        && let Some(second) = chars.next()
        && second.is_ascii_digit()
    {
        return false;
    }

    // Rest must be alphanumeric, dot, or underscore
    for ch in s.chars().skip(1) {
        if !ch.is_ascii_alphanumeric() && ch != '.' && ch != '_' {
            return false;
        }
    }

    true
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
    fn test_parse_namespace_export_multiline_with_comments() {
        let content = r##"
export(
    "as.mask",
    # Listing grobs and viewports
    "grid.ls",
    "grid.grep",
    "literal#hash"
)
"##;
        let exports = parse_namespace_content(content);
        assert_eq!(
            exports,
            vec!["as.mask", "grid.ls", "grid.grep", "literal#hash"]
        );
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

    // S4 export directives (issue #474): exportMethods / exportClasses are
    // plain exports (matching `getNamespaceExports`); exportClassPattern is a
    // pattern marker like exportPattern.

    #[test]
    fn test_parse_namespace_export_methods_single() {
        let content = "exportMethods(spTransform)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["spTransform"]);
    }

    #[test]
    fn test_parse_namespace_export_methods_multiple_multiline() {
        // Models sp's exportMethods block (issue #474).
        let content = r#"
exportMethods(
    spTransform,
    spChFIDs,
    "spChFIDs<-"
)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["spTransform", "spChFIDs", "spChFIDs<-"]);
    }

    #[test]
    fn test_parse_namespace_export_classes() {
        // Models sp's exportClasses block (issue #474).
        let content = "exportClasses(CRS, Spatial, SpatialPolygons)";
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["CRS", "Spatial", "SpatialPolygons"]);
    }

    #[test]
    fn test_parse_namespace_export_class_pattern_is_marker() {
        // exportClassPattern is treated like exportPattern: stored as a marker
        // so the resolver flips `has_export_pattern` and consults R/INDEX.
        let content = r#"exportClassPattern("^Spatial")"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["__PATTERN__:^Spatial"]);
    }

    #[test]
    fn test_parse_namespace_export_prefix_not_swallowed_by_export() {
        // The `export` branch must not swallow exportMethods/exportClasses.
        let content = r#"
export(plainFn)
exportMethods(gUnaryUnion)
exportClasses(Spatial)
"#;
        let exports = parse_namespace_content(content);
        assert_eq!(exports, vec!["plainFn", "gUnaryUnion", "Spatial"]);
    }

    #[test]
    fn test_parse_namespace_s4_directives_no_export_pattern() {
        // A package exporting only via export()+exportMethods()+exportClasses()
        // (no exportPattern) — the sp/maptools shape. None of the names should
        // be lost, and no `__PATTERN__:` marker should appear.
        let content = r#"
export(rebuild_CRS)
exportClasses(CRS, Spatial)
exportMethods(spTransform, spChFIDs)
"#;
        let exports = parse_namespace_content(content);
        assert!(!exports.iter().any(|e| e.starts_with("__PATTERN__:")));
        for name in ["rebuild_CRS", "CRS", "Spatial", "spTransform", "spChFIDs"] {
            assert!(
                exports.contains(&name.to_string()),
                "missing export {name}: {exports:?}"
            );
        }
    }

    #[test]
    fn test_parse_namespace_empty_s4_directives() {
        // Empty S4 directives produce no exports, mirroring empty `export()`.
        let content = "exportMethods()\nexportClasses()\nexportClassPattern()";
        let exports = parse_namespace_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_namespace_s4_directives_coexist_with_export_pattern() {
        // When a package mixes exportPattern with S4 directives, the plain S4
        // names are still collected AND the pattern marker is present (so
        // `has_export_pattern` still triggers R/INDEX expansion).
        let content = r#"
export(plainFn)
exportPattern("^[^.]")
exportMethods(spTransform)
exportClasses(Spatial)
"#;
        let exports = parse_namespace_content(content);
        assert!(exports.contains(&"plainFn".to_string()));
        assert!(exports.contains(&"spTransform".to_string()));
        assert!(exports.contains(&"Spatial".to_string()));
        assert!(exports.contains(&"__PATTERN__:^[^.]".to_string()));
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
        let content = "Package: mypackage\nDepends: R (>= 4.0), dplyr (>= 1.0.0), ggplot2 (>= 3.0)\nVersion: 1.0.0";
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

    // Tests for parse_description_metadata (Title/Description free-text fields)

    #[test]
    fn test_parse_description_metadata_title_and_description() {
        let content = "\
Package: dplyr
Title: A Grammar of Data Manipulation
Version: 1.1.4
Description: A fast, consistent tool for working with data frame like
    objects, both in memory and out of memory.
License: MIT
";
        let meta = parse_description_metadata_str(content);
        assert_eq!(
            meta.title.as_deref(),
            Some("A Grammar of Data Manipulation")
        );
        // Continuation line is folded into a single line with one space.
        assert_eq!(
            meta.description.as_deref(),
            Some(
                "A fast, consistent tool for working with data frame like objects, both in memory and out of memory."
            )
        );
    }

    #[test]
    fn test_parse_description_metadata_missing_fields() {
        // Free-text fields are NOT comma-split or version-stripped, so a Title
        // containing a comma stays intact; a missing Description yields None.
        let content = "Package: tools\nTitle: Tools, Helpers, and Utilities\n";
        let meta = parse_description_metadata_str(content);
        assert_eq!(meta.title.as_deref(), Some("Tools, Helpers, and Utilities"));
        assert_eq!(meta.description, None);
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
    fn test_find_matching_paren_doctest() {
        let s = "a,b)";
        // treating the input as the content after an opening '(', the closing ')' is at byte index 3
        assert_eq!(find_matching_paren(s), Some(3));
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
    fn test_normalize_strips_comments() {
        let content = "# comment\nexport(foo)";
        let normalized = normalize_multiline_directives(content);
        assert!(
            !normalized.contains("# comment"),
            "comments should be stripped"
        );
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
    /// ```ignore
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

    // ============================================================================
    // Tests for INDEX File Parsing
    // ============================================================================

    #[test]
    fn test_parse_index_simple() {
        let content = r#"across                  Apply a function across columns
arrange                 Order rows using column values
mutate                  Create or modify columns"#;
        let exports = parse_index_content(content);
        assert_eq!(exports, vec!["across", "arrange", "mutate"]);
    }

    #[test]
    fn test_parse_index_multiline_description() {
        let content = r#"consecutive_id          Generate a unique identifier for consecutive
                        combinations
context                 Information about the "current" group or
                        variable
copy_to                 Copy a local data frame to a remote src"#;
        let exports = parse_index_content(content);
        assert_eq!(exports, vec!["consecutive_id", "context", "copy_to"]);
    }

    #[test]
    fn test_parse_index_empty() {
        let content = "";
        let exports = parse_index_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_index_only_whitespace() {
        let content = "   \n\t\n   ";
        let exports = parse_index_content(content);
        assert!(exports.is_empty());
    }

    #[test]
    fn test_parse_index_with_dots() {
        let content = r#"data.frame              Create a data frame
as.character            Convert to character
is.null                 Check for NULL"#;
        let exports = parse_index_content(content);
        assert_eq!(exports, vec!["data.frame", "as.character", "is.null"]);
    }

    #[test]
    fn test_parse_index_with_underscores() {
        let content = r#"read_csv                Read CSV file
write_csv               Write CSV file
parse_number            Parse numbers"#;
        let exports = parse_index_content(content);
        assert_eq!(exports, vec!["read_csv", "write_csv", "parse_number"]);
    }

    #[test]
    fn test_parse_index_operators() {
        // Backtick-quoted operators should be valid
        let content = r#"`%>%`                   Pipe operator
`%<>%`                  Assignment pipe"#;
        let exports = parse_index_content(content);
        assert_eq!(exports, vec!["`%>%`", "`%<>%`"]);
    }

    #[test]
    fn test_parse_index_ignores_invalid_identifiers() {
        // Lines that don't start with valid R identifiers should be skipped
        let content = r#"valid_func              A valid function
123invalid              Invalid - starts with number
valid2                  Another valid one"#;
        let exports = parse_index_content(content);
        assert_eq!(exports, vec!["valid_func", "valid2"]);
    }

    #[test]
    fn test_is_valid_r_identifier() {
        // Valid identifiers
        assert!(is_valid_r_identifier("foo"));
        assert!(is_valid_r_identifier("foo_bar"));
        assert!(is_valid_r_identifier("foo.bar"));
        assert!(is_valid_r_identifier("foo123"));
        assert!(is_valid_r_identifier(".foo"));
        assert!(is_valid_r_identifier("`%>%`"));
        assert!(is_valid_r_identifier("data.frame"));
        assert!(is_valid_r_identifier("as.character"));

        // Invalid identifiers
        assert!(!is_valid_r_identifier(""));
        assert!(!is_valid_r_identifier("123foo")); // starts with digit
        assert!(!is_valid_r_identifier(".123")); // dot followed by digit
        assert!(!is_valid_r_identifier("foo-bar")); // contains hyphen
        assert!(!is_valid_r_identifier("foo bar")); // contains space
    }

    // ---- parse_data_symbols (issue #276) ---------------------------------

    #[tokio::test]
    async fn test_parse_data_symbols_no_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let names = parse_data_symbols(dir.path()).await;
        assert!(names.is_empty());
    }

    #[tokio::test]
    async fn test_parse_data_symbols_from_datalist() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(
            data_dir.join("datalist"),
            "# comment line\nfoo\nbar: bar1 bar2\n\nbaz\n",
        )
        .unwrap();

        let mut names = parse_data_symbols(dir.path()).await;
        names.sort();
        assert_eq!(names, vec!["bar", "bar1", "bar2", "baz", "foo"]);
    }

    #[tokio::test]
    async fn test_parse_data_symbols_from_individual_files() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        // Individual data files in various supported formats.
        fs::write(data_dir.join("alpha.rda"), b"").unwrap();
        fs::write(data_dir.join("beta.RData"), b"").unwrap();
        fs::write(data_dir.join("gamma.csv"), b"").unwrap();
        fs::write(data_dir.join("delta.tab"), b"").unwrap();
        fs::write(data_dir.join("epsilon.txt"), b"").unwrap();
        fs::write(data_dir.join("zeta.R"), b"").unwrap();
        // Files to skip
        fs::write(data_dir.join("Rdata.rdb"), b"").unwrap();
        fs::write(data_dir.join("Rdata.rdx"), b"").unwrap();
        fs::write(data_dir.join("Rdata.rds"), b"").unwrap();
        // Unknown extension should be ignored
        fs::write(data_dir.join("ignored.bin"), b"").unwrap();
        // Invalid identifier (starts with a digit) should be skipped
        fs::write(data_dir.join("9bad.rda"), b"").unwrap();

        let names = parse_data_symbols(dir.path()).await;
        let mut got = names;
        got.sort();
        assert_eq!(
            got,
            vec!["alpha", "beta", "delta", "epsilon", "gamma", "zeta"]
        );
    }

    #[tokio::test]
    async fn test_parse_data_symbols_datalist_and_files_combined() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(data_dir.join("datalist"), "foo\nbar: bar1\n").unwrap();
        fs::write(data_dir.join("baz.rda"), b"").unwrap();

        let mut names = parse_data_symbols(dir.path()).await;
        names.sort();
        assert_eq!(names, vec!["bar", "bar1", "baz", "foo"]);
    }

    #[test]
    fn test_data_file_stem_edge_cases() {
        // Extension classification helper. Keep the input groups identical to
        // the production usage so the tests pin actual behavior.
        let serialized = &["rda", "rdata", "r"][..];
        let tabular = &["csv", "tab", "txt"][..];
        let compression = &["gz", "bz2", "xz"][..];
        let stem =
            |name: &str| data_file_stem(name, serialized, tabular, compression).map(str::to_owned);

        // Bare names (no dot) are not data files.
        assert_eq!(stem("README"), None);
        assert_eq!(stem(""), None);

        // Mixed-case outer extensions still classify correctly.
        assert_eq!(stem("foo.RData"), Some("foo".into()));
        assert_eq!(stem("foo.RDA"), Some("foo".into()));
        assert_eq!(stem("foo.CSV"), Some("foo".into()));

        // Cased compression wrappers around a valid tabular extension.
        assert_eq!(stem("foo.CSV.GZ"), Some("foo".into()));
        assert_eq!(stem("foo.TAB.Xz"), Some("foo".into()));

        // Compression wrapper around a non-tabular inner extension rejected.
        assert_eq!(stem("foo.RData.gz"), None);
        assert_eq!(stem("foo.R.gz"), None);

        // Unrelated `.tar.gz` files don't masquerade as data.
        assert_eq!(stem("foo.tar.gz"), None);

        // No inner extension under a compression wrapper rejected.
        assert_eq!(stem("foo.gz"), None);
        assert_eq!(stem(".gz"), None);

        // Hidden files with a recognized extension still parse.
        assert_eq!(stem(".hidden.csv"), Some(".hidden".into()));
    }

    #[tokio::test]
    async fn test_parse_data_symbols_compressed_extensions() {
        // Tabular data files commonly ship under a compression layer; R will
        // still load `population.csv.gz` as `data(population)`.
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir).unwrap();
        fs::write(data_dir.join("population.csv.gz"), b"").unwrap();
        fs::write(data_dir.join("trees.txt.bz2"), b"").unwrap();
        fs::write(data_dir.join("readings.tab.xz"), b"").unwrap();
        // Compression layer with a non-data inner extension stays ignored.
        fs::write(data_dir.join("ignored.bin.gz"), b"").unwrap();
        // R does NOT load `.rda` / `.R` files through a compression wrapper —
        // those formats already have their own compression / evaluation
        // semantics. Reject them so we don't mint phantom dataset names.
        fs::write(data_dir.join("phantom.rda.gz"), b"").unwrap();
        fs::write(data_dir.join("scripted.R.gz"), b"").unwrap();
        // Double-compressed files (rare) are intentionally not unwrapped.
        fs::write(data_dir.join("oddly.csv.gz.gz"), b"").unwrap();

        let mut names = parse_data_symbols(dir.path()).await;
        names.sort();
        assert_eq!(names, vec!["population", "readings", "trees"]);
    }

    #[tokio::test]
    async fn test_parse_data_symbols_rejects_parent_dir_components() {
        // Mirrors the existing guard in `parse_index_exports`.
        let path = PathBuf::from("/tmp/some/../etc");
        let names = parse_data_symbols(&path).await;
        assert!(names.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_parse_data_symbols_skips_symlinked_data_dir() {
        // If `data/` itself is a symlink, refuse to follow it — a malformed or
        // tampered installed-package tree must not be able to leak arbitrary
        // names into base_exports.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("attacker.rda"), b"").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("data")).unwrap();

        let names = parse_data_symbols(dir.path()).await;
        assert!(
            names.is_empty(),
            "symlinked data/ dir must not contribute symbols, got: {:?}",
            names
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_parse_data_symbols_skips_symlinked_datalist() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir(&data_dir).unwrap();

        let outside = tempfile::tempdir().unwrap();
        let evil = outside.path().join("evil_datalist");
        fs::write(&evil, "attacker_symbol\n").unwrap();
        std::os::unix::fs::symlink(&evil, data_dir.join("datalist")).unwrap();

        let names = parse_data_symbols(dir.path()).await;
        assert!(
            !names.contains(&"attacker_symbol".to_string()),
            "symlinked datalist must not be followed, got: {:?}",
            names
        );
    }
}
