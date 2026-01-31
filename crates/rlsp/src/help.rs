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

/// Get help for a topic by calling R as a subprocess
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
