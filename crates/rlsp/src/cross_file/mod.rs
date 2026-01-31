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

/// Extract cross-file metadata from R source code (Requirement 0.1)
/// Combines directive parsing with AST-detected source() calls
pub fn extract_metadata(content: &str) -> CrossFileMetadata {
    log::trace!("Extracting cross-file metadata from content ({} bytes)", content.len());
    
    // Parse directives first
    let mut meta = directive::parse_directives(content);
    
    // Parse AST for source() calls using thread-local parser for efficiency
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
    } else {
        log::warn!("Failed to parse R code with tree-sitter during metadata extraction");
    }
    
    log::trace!(
        "Metadata extraction complete: {} total sources ({} from directives, {} from AST), {} backward directives",
        meta.sources.len(),
        meta.sources.iter().filter(|s| s.is_directive).count(),
        meta.sources.iter().filter(|s| !s.is_directive).count(),
        meta.sourced_by.len()
    );
    
    meta
}