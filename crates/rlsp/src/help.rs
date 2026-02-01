// help.rs - Minimal R help system for static LSP
//
// This module calls R as a subprocess to get help documentation.
// It's "static" in that it doesn't embed R, but can still access help.

use std::process::Command;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Cache for help content
pub struct HelpCache {
    cache: Arc<RwLock<HashMap<String, Option<String>>>>,
}

impl HelpCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get(&self, topic: &str) -> Option<Option<String>> {
        self.cache.read().ok()?.get(topic).cloned()
    }

    pub fn insert(&self, topic: String, content: Option<String>) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(topic, content);
        }
    }
}

impl Default for HelpCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Fetches help text for an R topic by invoking R and returns the rendered help if available.
///
/// Returns `Some(String)` containing the help text when R executes successfully, the output is not empty,
/// and the output does not contain the phrase "No documentation". Returns `None` otherwise.
///
/// # Examples
///
/// ```no_run
/// // Attempt to get help for `mean` from the base package
/// if let Some(text) = rlsp::help::get_help("mean", Some("base")) {
///     assert!(text.contains("Usage:"));
/// }
/// ```
pub fn get_help(topic: &str, package: Option<&str>) -> Option<String> {
    let r_code = if let Some(pkg) = package {
        format!(
            "cat(paste(capture.output(tools::Rd2txt(utils:::.getHelpFile(help('{}', package='{}')), options=list(underline_titles=FALSE))), collapse='\\n'))",
            topic, pkg
        )
    } else {
        format!(
            "cat(paste(capture.output(tools::Rd2txt(utils:::.getHelpFile(help('{}')), options=list(underline_titles=FALSE))), collapse='\\n'))",
            topic
        )
    };

    let output = Command::new("R")
        .args(["--slave", "--no-save", "-e", &r_code])
        .output()
        .ok()?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if !text.trim().is_empty() && !text.contains("No documentation") {
            return Some(text);
        }
    }

    None
}

/// Extracts the first function signature from R help text.
///
/// Scans the "Usage:" section of R help output and returns the first available
/// signature, preferring a generic signature over an S3 method. Handles
/// multi-line signatures and stops at the next section header (a non-parenthesized
/// line ending with ':').
///
/// # Returns
///
/// `Some(String)` containing the joined signature lines if a signature is found,
/// `None` otherwise.
///
/// # Examples
///
/// ```
/// let help = r#"
/// Arithmetic Mean
///
/// Description:
///     Generic function for the (trimmed) arithmetic mean.
///
/// Usage:
///     mean(x, ...)
///
///     ## Default S3 method:
///     mean(x, trim = 0, na.rm = FALSE, ...)
///
/// Arguments:
///     ...
/// "#;
///
/// assert_eq!(extract_signature_from_help(help), Some("mean(x, ...)".to_string()));
/// ```
pub fn extract_signature_from_help(help_text: &str) -> Option<String> {
    let lines: Vec<&str> = help_text.lines().collect();
    
    // Find the "Usage:" section
    let usage_idx = lines.iter().position(|line| line.trim() == "Usage:")?;
    
    // Collect lines after "Usage:" until we hit another section (like "Arguments:")
    let mut signature_lines: Vec<&str> = Vec::new();
    let mut in_s3_comment = false;
    
    for line in lines.iter().skip(usage_idx + 1) {
        let trimmed = line.trim();
        
        // Stop at the next section header (ends with ":" and doesn't contain parentheses)
        if !trimmed.is_empty() && trimmed.ends_with(':') && !trimmed.contains('(') {
            break;
        }
        
        // Check for S3 method comments
        if trimmed.starts_with("## S3 method") || trimmed.starts_with("## Default S3 method") {
            // If we already have a non-S3 signature, we're done
            if !signature_lines.is_empty() {
                break;
            }
            in_s3_comment = true;
            continue;
        }
        
        // Skip empty lines
        if trimmed.is_empty() {
            // If we have a complete signature (ends with ')'), we're done
            if !signature_lines.is_empty() {
                let last = signature_lines.last().unwrap_or(&"");
                if last.ends_with(')') {
                    break;
                }
            }
            in_s3_comment = false;
            continue;
        }
        
        // If we're after an S3 comment and don't have a signature yet, take this one
        // Otherwise, if we're not after an S3 comment, this is a generic signature
        if in_s3_comment && signature_lines.is_empty() {
            // This is an S3 method signature - take it as fallback
            signature_lines.push(trimmed);
            if trimmed.ends_with(')') {
                // Complete S3 signature, but keep looking for a generic one
                // Actually, let's just return this since it's useful
                break;
            }
        } else if !in_s3_comment {
            // This is a generic signature
            signature_lines.push(trimmed);
            if trimmed.ends_with(')') {
                break;
            }
        }
    }
    
    if signature_lines.is_empty() {
        return None;
    }
    
    // Join multi-line signatures
    let signature = signature_lines.join("\n");
    
    Some(signature)
}

/// Retrieves the function signature for a topic within a package.
///
/// Attempts to fetch the topic's help text and extract the first available function signature from its Usage section.
///
/// # Returns
///
/// `Some(signature)` if a signature is found, `None` otherwise.
///
/// # Examples
///
/// ```
/// let sig = get_function_signature("mean", "base");
/// if let Some(s) = sig {
///     println!("{}", s);
/// }
/// ```
pub fn get_function_signature(topic: &str, package: &str) -> Option<String> {
    let help_text = get_help(topic, Some(package))?;
    extract_signature_from_help(&help_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_signature_simple() {
        let help_text = r#"Arithmetic Mean

Description:

     Generic function for the (trimmed) arithmetic mean.

Usage:

     mean(x, ...)
     
     ## Default S3 method:
     mean(x, trim = 0, na.rm = FALSE, ...)
     
Arguments:

       x: an R object.
"#;
        
        let sig = extract_signature_from_help(help_text);
        assert_eq!(sig, Some("mean(x, ...)".to_string()));
    }

    #[test]
    fn test_extract_signature_multiline() {
        let help_text = r#"Create, modify, and delete columns

Description:

     'mutate()' creates new columns.

Usage:

     mutate(.data, ...)
     
     ## S3 method for class 'data.frame'
     mutate(
       .data,
       ...,
       .by = NULL
     )
     
Arguments:

   .data: A data frame.
"#;
        
        let sig = extract_signature_from_help(help_text);
        assert_eq!(sig, Some("mutate(.data, ...)".to_string()));
    }

    #[test]
    fn test_extract_signature_no_usage() {
        let help_text = r#"Some Topic

Description:

     Some description without usage section.
"#;
        
        let sig = extract_signature_from_help(help_text);
        assert_eq!(sig, None);
    }

    #[test]
    fn test_extract_signature_only_s3_method() {
        // Some functions only have S3 method signatures
        // In this case, we should still return the S3 method signature
        // since it's useful information for the user
        let help_text = r#"Some Function

Description:

     A function.

Usage:

     ## S3 method for class 'foo'
     bar(x, y = 1)
     
Arguments:

       x: an object.
"#;
        
        // Should return the S3 method signature since there's no generic
        let sig = extract_signature_from_help(help_text);
        assert_eq!(sig, Some("bar(x, y = 1)".to_string()));
    }
}