//
// cross_file/mod.rs
//
// Cross-file awareness for Rlsp
//

// Allow dead code for infrastructure that's implemented for future use
#![allow(dead_code)]

pub mod background_indexer;
pub mod cache;
pub mod config;
pub mod content_provider;
pub mod dependency;
pub mod directive;
pub mod file_cache;
pub mod parent_resolve;
pub mod path_resolve;
pub mod revalidation;
pub mod scope;
pub mod source_detect;
pub mod types;
pub mod workspace_index;

#[cfg(test)]
mod property_tests;

#[cfg(test)]
pub mod integration_tests;

pub use background_indexer::*;
pub use cache::*;
pub use config::*;
#[allow(unused_imports)]
pub use content_provider::*;
pub use dependency::*;
#[allow(unused_imports)]
pub use directive::*;
pub use file_cache::*;
#[allow(unused_imports)]
pub use parent_resolve::*;
#[allow(unused_imports)]
pub use path_resolve::*;
pub use revalidation::*;
pub use scope::*;
#[allow(unused_imports)]
pub use source_detect::*;
pub use types::*;
pub use workspace_index::*;

/// Extract cross-file metadata from R source by combining directive parsing with AST-detected `source()` and library-related calls.
///
/// Directive-derived `source` entries take precedence over AST-detected `source()` calls when they occur on the same line. When a thread-local parser is available the function also detects `library()`, `require()`, and `loadNamespace()` calls and records them in `library_calls`; if parsing fails those AST-derived detections are skipped.
///
/// # Returns
///
/// A `CrossFileMetadata` containing collected `sources`, `sourced_by` entries, and `library_calls`. `sources` and `library_calls` are sorted by document order (line, column).
///
/// # Examples
///
/// ```
/// let content = r#"
/// #> sourceline: helper.R
/// source('other.R')
/// library(pkg)
/// "#;
/// let meta = cross_file::extract_metadata(content);
/// assert!(meta.sources.len() >= 1);
/// assert!(meta.library_calls.iter().any(|lc| lc.name == "pkg"));
/// ```
pub fn extract_metadata(content: &str) -> CrossFileMetadata {
    log::trace!("Extracting cross-file metadata from content ({} bytes)", content.len());
    
    // Parse directives first
    let mut meta = directive::parse_directives(content);
    
    // Parse AST for source() calls and library() calls using thread-local parser for efficiency
    if let Some(tree) = crate::parser_pool::with_parser(|parser| parser.parse(content, None)) {
        let detected = source_detect::detect_source_calls(&tree, content);
        
        // Merge detected source() calls with directive sources
        // Directive sources take precedence (Requirement 6.8)
        for source in detected {
            // Check if there's already a directive at the same line
            let has_directive = meta.sources.iter().any(|s| {
                s.is_directive && s.line == source.line
            });
            if !has_directive {
                meta.sources.push(source);
            }
        }
        
        // Sort by line number for consistent ordering
        meta.sources.sort_by_key(|s| (s.line, s.column));
        
        // Detect library(), require(), loadNamespace() calls (Requirement 1.8)
        let mut library_calls = source_detect::detect_library_calls(&tree, content);
        // Sort by line/column for document order (Requirement 1.8)
        library_calls.sort_by_key(|lc| (lc.line, lc.column));
        meta.library_calls = library_calls;
    } else {
        log::warn!("Failed to parse R code with tree-sitter during metadata extraction");
    }
    
    log::trace!(
        "Metadata extraction complete: {} total sources ({} from directives, {} from AST), {} backward directives, {} library calls",
        meta.sources.len(),
        meta.sources.iter().filter(|s| s.is_directive).count(),
        meta.sources.iter().filter(|s| !s.is_directive).count(),
        meta.sourced_by.len(),
        meta.library_calls.len()
    );
    
    meta
}