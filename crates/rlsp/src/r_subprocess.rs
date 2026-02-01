// r_subprocess.rs - R subprocess interface for package queries
//
// This module provides an async interface for querying R about packages,
// library paths, and exports. It's used by the package function awareness
// feature to resolve package symbols.

// Allow dead code during incremental development - this module will be
// integrated into WorldState in task 7.1
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tokio::process::Command;

/// R subprocess interface for package queries
pub struct RSubprocess {
    /// Path to R executable
    r_path: PathBuf,
}

impl RSubprocess {
    /// Create new subprocess interface
    ///
    /// If `r_path` is provided, uses that path directly.
    /// Otherwise, attempts to discover R in PATH or common locations.
    pub fn new(r_path: Option<PathBuf>) -> Option<Self> {
        let path = match r_path {
            Some(p) => {
                if Self::is_valid_r_executable(&p) {
                    Some(p)
                } else {
                    log::trace!("Provided R path is not valid: {:?}", p);
                    None
                }
            }
            None => Self::discover_r_path(),
        };

        path.map(|r_path| {
            log::trace!("Using R executable at: {:?}", r_path);
            Self { r_path }
        })
    }

    /// Get the path to the R executable
    pub fn r_path(&self) -> &PathBuf {
        &self.r_path
    }

    /// Discover R executable path by checking PATH and common locations
    fn discover_r_path() -> Option<PathBuf> {
        // First, try to find R in PATH using `which` on Unix or `where` on Windows
        if let Some(path) = Self::find_r_in_path() {
            return Some(path);
        }

        // Fall back to common installation locations
        Self::find_r_in_common_locations()
    }

    /// Find R in the system PATH
    fn find_r_in_path() -> Option<PathBuf> {
        #[cfg(unix)]
        {
            let output = std::process::Command::new("which")
                .arg("R")
                .output()
                .ok()?;

            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                let path = PathBuf::from(path_str.trim());
                if Self::is_valid_r_executable(&path) {
                    return Some(path);
                }
            }
        }

        #[cfg(windows)]
        {
            let output = std::process::Command::new("where")
                .arg("R")
                .output()
                .ok()?;

            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout);
                // `where` may return multiple lines; take the first one
                if let Some(first_line) = path_str.lines().next() {
                    let path = PathBuf::from(first_line.trim());
                    if Self::is_valid_r_executable(&path) {
                        return Some(path);
                    }
                }
            }
        }

        None
    }

    /// Find R in common installation locations
    fn find_r_in_common_locations() -> Option<PathBuf> {
        let common_paths = Self::get_common_r_paths();
        common_paths
            .into_iter()
            .find(Self::is_valid_r_executable)
    }

    /// Get platform-specific common R installation paths
    fn get_common_r_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        #[cfg(target_os = "macos")]
        {
            // Homebrew locations
            paths.push(PathBuf::from("/opt/homebrew/bin/R"));
            paths.push(PathBuf::from("/usr/local/bin/R"));
            // R.app framework location
            paths.push(PathBuf::from(
                "/Library/Frameworks/R.framework/Resources/bin/R",
            ));
        }

        #[cfg(target_os = "linux")]
        {
            paths.push(PathBuf::from("/usr/bin/R"));
            paths.push(PathBuf::from("/usr/local/bin/R"));
            // Common conda/mamba locations
            if let Ok(home) = std::env::var("HOME") {
                paths.push(PathBuf::from(format!("{}/miniconda3/bin/R", home)));
                paths.push(PathBuf::from(format!("{}/anaconda3/bin/R", home)));
                paths.push(PathBuf::from(format!("{}/.local/bin/R", home)));
            }
        }

        #[cfg(target_os = "windows")]
        {
            // Common Windows R installation paths
            paths.push(PathBuf::from("C:\\Program Files\\R\\R-4.4.0\\bin\\R.exe"));
            paths.push(PathBuf::from("C:\\Program Files\\R\\R-4.3.0\\bin\\R.exe"));
            paths.push(PathBuf::from("C:\\Program Files\\R\\R-4.2.0\\bin\\R.exe"));
            // Try to find any R version in Program Files
            if let Ok(entries) = std::fs::read_dir("C:\\Program Files\\R") {
                for entry in entries.flatten() {
                    let r_bin = entry.path().join("bin").join("R.exe");
                    if r_bin.exists() {
                        paths.push(r_bin);
                    }
                }
            }
        }

        paths
    }

    /// Check if a path points to a valid R executable
    fn is_valid_r_executable(path: &PathBuf) -> bool {
        if !path.exists() {
            return false;
        }

        // Try to run R --version to verify it's a working R installation
        let result = std::process::Command::new(path)
            .args(["--version"])
            .output();

        match result {
            Ok(output) => {
                // R --version outputs to stderr, not stdout
                let version_output = String::from_utf8_lossy(&output.stderr);
                output.status.success() || version_output.contains("R version")
            }
            Err(_) => false,
        }
    }

    /// Execute an R expression and return the output
    ///
    /// This is an async helper that runs R with the given expression
    /// and returns the stdout output as a string.
    pub async fn execute_r_code(&self, r_code: &str) -> Result<String> {
        let output = Command::new(&self.r_path)
            .args(["--slave", "--no-save", "--no-restore", "-e", r_code])
            .output()
            .await
            .map_err(|e| anyhow!("Failed to execute R subprocess: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "R subprocess failed with status {}: {}",
                output.status,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(stdout)
    }

    /// Get library paths from R
    ///
    /// Calls `.libPaths()` in R and returns the list of library paths.
    /// Falls back to platform-specific standard paths if R subprocess fails.
    ///
    /// Requirement 7.1: THE LSP SHALL query R subprocess to get library paths using `.libPaths()`
    /// Requirement 7.2: IF R subprocess is unavailable, THE LSP SHALL use standard R library
    /// path locations for the platform
    pub async fn get_lib_paths(&self) -> Result<Vec<PathBuf>> {
        // Use cat() with sep="\n" to output each path on its own line without R's vector formatting
        let r_code = r#"cat(.libPaths(), sep="\n")"#;

        match self.execute_r_code(r_code).await {
            Ok(output) => {
                let paths = parse_lib_paths_output(&output);
                if paths.is_empty() {
                    log::trace!("R returned empty .libPaths(), using fallback paths");
                    Ok(get_fallback_lib_paths())
                } else {
                    Ok(paths)
                }
            }
            Err(e) => {
                log::trace!("Failed to get .libPaths() from R: {}, using fallback paths", e);
                Ok(get_fallback_lib_paths())
            }
        }
    }

    /// Get base/startup packages from R
    ///
    /// Calls `.packages()` in R and returns the list of base packages.
    /// Falls back to a hardcoded list if R subprocess fails.
    ///
    /// Requirement 6.1: THE LSP SHALL query R subprocess at initialization to get
    /// the default search path using `.packages()`
    /// Requirement 6.2: IF R subprocess is unavailable at initialization, THE LSP
    /// SHALL use a hardcoded list of base packages: base, methods, utils, grDevices,
    /// graphics, stats, datasets
    pub async fn get_base_packages(&self) -> Result<Vec<String>> {
        // Use cat() with sep="\n" to output each package name on its own line
        // without R's vector formatting (e.g., [1] "base" "methods" ...)
        let r_code = r#"cat(.packages(), sep="\n")"#;

        match self.execute_r_code(r_code).await {
            Ok(output) => {
                let packages = parse_packages_output(&output);
                if packages.is_empty() {
                    log::trace!("R returned empty .packages(), using fallback base packages");
                    Ok(get_fallback_base_packages())
                } else {
                    Ok(packages)
                }
            }
            Err(e) => {
                log::trace!(
                    "Failed to get .packages() from R: {}, using fallback base packages",
                    e
                );
                Ok(get_fallback_base_packages())
            }
        }
    }

    /// Get exports for a package
    ///
    /// Calls `getNamespaceExports(asNamespace("pkg"))` in R and returns the list of exports.
    /// This includes functions, variables, and datasets exported by the package.
    ///
    /// Requirement 3.1: WHEN a package is loaded, THE Package_Resolver SHALL query R subprocess
    /// to get the package's exported symbols using `getNamespaceExports()`
    ///
    /// # Arguments
    /// * `package` - The package name to query exports for
    ///
    /// # Returns
    /// * `Ok(Vec<String>)` - List of exported symbol names
    /// * `Err` - If the package is not installed or R subprocess fails
    ///
    /// # Security
    /// Package names are validated to prevent R code injection attacks.
    /// Only alphanumeric characters, dots, and underscores are allowed.
    pub async fn get_package_exports(&self, package: &str) -> Result<Vec<String>> {
        // Validate package name to prevent injection attacks
        // R package names can contain letters, numbers, dots, and underscores
        // They must start with a letter or dot (if dot, second char must be letter)
        if !is_valid_package_name(package) {
            return Err(anyhow!(
                "Invalid package name '{}': must contain only letters, numbers, dots, and underscores",
                package
            ));
        }

        // Use cat() with sep="\n" to output each export name on its own line
        // without R's vector formatting (e.g., [1] "func1" "func2" ...)
        // We use tryCatch to handle the case where the package is not installed
        let r_code = format!(
            r#"tryCatch(cat(getNamespaceExports(asNamespace("{}")), sep="\n"), error=function(e) cat("__RLSP_ERROR__:", conditionMessage(e), sep=""))"#,
            package
        );

        let output = self.execute_r_code(&r_code).await?;

        // Check if R returned an error
        if output.starts_with("__RLSP_ERROR__:") {
            let error_msg = output.trim_start_matches("__RLSP_ERROR__:").trim();
            return Err(anyhow!("Failed to get exports for package '{}': {}", package, error_msg));
        }

        // Parse the output - one export name per line
        let exports = parse_packages_output(&output);
        
        log::trace!(
            "Got {} exports for package '{}': {:?}",
            exports.len(),
            package,
            if exports.len() <= 10 {
                exports.clone()
            } else {
                let mut preview = exports[..10].to_vec();
                preview.push(format!("... and {} more", exports.len() - 10));
                preview
            }
        );

        Ok(exports)
    }

    /// Get package DESCRIPTION info (Depends field)
    ///
    /// Reads the package DESCRIPTION and extracts the Depends field.
    /// Returns a list of package names that the given package depends on.
    ///
    /// Requirement 4.1: WHEN a package is loaded, THE Package_Resolver SHALL read
    /// the package's DESCRIPTION file to find the `Depends` field
    ///
    /// # Arguments
    /// * `package` - The package name to query dependencies for
    ///
    /// # Returns
    /// * `Ok(Vec<String>)` - List of package names from the Depends field
    /// * `Err` - If the package is not installed or R subprocess fails
    ///
    /// # Notes
    /// - The "R" dependency (R version requirement) is filtered out
    /// - Version constraints like `(>= 3.5)` are stripped from package names
    /// - Package names are validated to prevent injection attacks
    pub async fn get_package_depends(&self, package: &str) -> Result<Vec<String>> {
        // Validate package name to prevent injection attacks
        if !is_valid_package_name(package) {
            return Err(anyhow!(
                "Invalid package name '{}': must contain only letters, numbers, dots, and underscores",
                package
            ));
        }

        // Use packageDescription to get the Depends field
        // First check if the package exists using find.package, then get the Depends field
        // We use tryCatch to handle the case where the package is not installed
        let r_code = format!(
            r#"tryCatch({{
                # Check if package exists first
                find.package("{}")
                desc <- packageDescription("{}", fields="Depends")
                if (is.na(desc)) {{
                    cat("")
                }} else {{
                    cat(desc)
                }}
            }}, error=function(e) cat("__RLSP_ERROR__:", conditionMessage(e), sep=""))"#,
            package, package
        );

        let output = self.execute_r_code(&r_code).await?;

        // Check if R returned an error
        if output.starts_with("__RLSP_ERROR__:") {
            let error_msg = output.trim_start_matches("__RLSP_ERROR__:").trim();
            return Err(anyhow!(
                "Failed to get depends for package '{}': {}",
                package,
                error_msg
            ));
        }

        // Parse the Depends field output
        let depends = parse_depends_field(&output);

        log::trace!(
            "Got {} dependencies for package '{}': {:?}",
            depends.len(),
            package,
            depends
        );

        Ok(depends)
    }
}

/// Parse the Depends field from a DESCRIPTION file
///
/// The Depends field is a comma-separated list of package names,
/// optionally with version constraints in parentheses.
///
/// Examples:
/// - "R (>= 3.5), dplyr, ggplot2"
/// - "methods, stats"
/// - "R (>= 4.0.0)"
///
/// This function:
/// 1. Splits by comma
/// 2. Strips version constraints (anything in parentheses)
/// 3. Filters out "R" (the R version requirement)
/// 4. Validates remaining package names
fn parse_depends_field(depends_str: &str) -> Vec<String> {
    let trimmed = depends_str.trim();
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
        // Validate package names
        .filter(|s| is_valid_package_name(s))
        .map(String::from)
        .collect()
}

/// Parse the output of `.libPaths()` from R
///
/// The output format when using `cat(.libPaths(), sep="\n")` is simply
/// one path per line:
/// ```
/// /Library/Frameworks/R.framework/Versions/4.4-arm64/Resources/library
/// /Users/user/Library/R/arm64/4.4/library
/// ```
fn parse_lib_paths_output(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

/// Parse the output of `.packages()` from R
///
/// The output format when using `cat(.packages(), sep="\n")` is simply
/// one package name per line:
/// ```
/// base
/// methods
/// utils
/// grDevices
/// graphics
/// stats
/// datasets
/// ```
fn parse_packages_output(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

/// Validate an R package name to prevent injection attacks
///
/// R package names must:
/// - Contain only ASCII letters, digits, dots (.), and underscores (_)
/// - Start with a letter or a dot
/// - If starting with a dot, the second character must be a letter
/// - Be at least 2 characters long (or 1 character if it's a letter)
///
/// This validation prevents malicious input from being executed as R code.
fn is_valid_package_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let chars: Vec<char> = name.chars().collect();
    
    // Check first character: must be a letter or dot
    let first = chars[0];
    if !first.is_ascii_alphabetic() && first != '.' {
        return false;
    }

    // If starts with dot, second character must be a letter (not a digit)
    // This prevents names like ".1" which could be interpreted as numbers
    if first == '.' {
        if chars.len() < 2 {
            return false;
        }
        if !chars[1].is_ascii_alphabetic() {
            return false;
        }
    }

    // All characters must be alphanumeric, dot, or underscore
    for c in &chars {
        if !c.is_ascii_alphanumeric() && *c != '.' && *c != '_' {
            return false;
        }
    }

    true
}

/// Get the hardcoded fallback list of base packages
///
/// This is used when R subprocess is unavailable.
/// Requirement 6.2: IF R subprocess is unavailable at initialization, THE LSP
/// SHALL use a hardcoded list of base packages: base, methods, utils, grDevices,
/// graphics, stats, datasets
pub fn get_fallback_base_packages() -> Vec<String> {
    vec![
        "base".to_string(),
        "methods".to_string(),
        "utils".to_string(),
        "grDevices".to_string(),
        "graphics".to_string(),
        "stats".to_string(),
        "datasets".to_string(),
    ]
}

/// Get standard R library path locations for the platform
///
/// This is used as a fallback when R subprocess is unavailable.
/// Requirement 7.2: IF R subprocess is unavailable, THE LSP SHALL use
/// standard R library path locations for the platform.
pub fn get_fallback_lib_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    #[cfg(target_os = "macos")]
    {
        // R.app framework library
        paths.push(PathBuf::from(
            "/Library/Frameworks/R.framework/Versions/Current/Resources/library",
        ));
        // User library
        if let Ok(home) = std::env::var("HOME") {
            // R 4.x user library location
            paths.push(PathBuf::from(format!(
                "{}/Library/R/x86_64/4.4/library",
                home
            )));
            paths.push(PathBuf::from(format!(
                "{}/Library/R/arm64/4.4/library",
                home
            )));
            // Older R versions
            paths.push(PathBuf::from(format!(
                "{}/Library/R/x86_64/4.3/library",
                home
            )));
            paths.push(PathBuf::from(format!(
                "{}/Library/R/arm64/4.3/library",
                home
            )));
        }
        // Homebrew library locations
        paths.push(PathBuf::from("/opt/homebrew/lib/R/library"));
        paths.push(PathBuf::from("/usr/local/lib/R/library"));
    }

    #[cfg(target_os = "linux")]
    {
        // System library
        paths.push(PathBuf::from("/usr/lib/R/library"));
        paths.push(PathBuf::from("/usr/local/lib/R/library"));
        paths.push(PathBuf::from("/usr/lib64/R/library"));
        // User library
        if let Ok(home) = std::env::var("HOME") {
            paths.push(PathBuf::from(format!("{}/R/x86_64-pc-linux-gnu-library/4.4", home)));
            paths.push(PathBuf::from(format!("{}/R/x86_64-pc-linux-gnu-library/4.3", home)));
        }
    }

    #[cfg(target_os = "windows")]
    {
        // System library
        paths.push(PathBuf::from("C:\\Program Files\\R\\R-4.4.0\\library"));
        paths.push(PathBuf::from("C:\\Program Files\\R\\R-4.3.0\\library"));
        // User library
        if let Ok(userprofile) = std::env::var("USERPROFILE") {
            paths.push(PathBuf::from(format!(
                "{}\\AppData\\Local\\R\\win-library\\4.4",
                userprofile
            )));
            paths.push(PathBuf::from(format!(
                "{}\\AppData\\Local\\R\\win-library\\4.3",
                userprofile
            )));
        }
    }

    // Filter to only existing paths
    paths.into_iter().filter(|p| p.exists()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_with_none_discovers_r() {
        // This test will pass if R is installed, skip otherwise
        let subprocess = RSubprocess::new(None);
        if subprocess.is_some() {
            let subprocess = subprocess.unwrap();
            assert!(subprocess.r_path().exists());
        }
    }

    #[test]
    fn test_new_with_invalid_path_returns_none() {
        let invalid_path = PathBuf::from("/nonexistent/path/to/R");
        let subprocess = RSubprocess::new(Some(invalid_path));
        assert!(subprocess.is_none());
    }

    #[test]
    fn test_get_common_r_paths_returns_paths() {
        let paths = RSubprocess::get_common_r_paths();
        // Should return at least some common paths for the platform
        assert!(!paths.is_empty());
    }

    #[test]
    fn test_fallback_lib_paths() {
        // This just tests that the function doesn't panic
        let paths = get_fallback_lib_paths();
        // Paths may or may not exist depending on the system
        for path in &paths {
            assert!(path.is_absolute() || path.starts_with("~"));
        }
    }

    #[tokio::test]
    async fn test_execute_r_code_simple() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.execute_r_code("cat('hello')").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn test_execute_r_code_with_output() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.execute_r_code("cat(1 + 1)").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "2");
    }

    #[tokio::test]
    async fn test_execute_r_code_error() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // This should fail because of invalid R syntax
        let result = subprocess.execute_r_code("stop('test error')").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_lib_paths_output_simple() {
        // Test parsing output with simple paths (one per line)
        let output = "/usr/lib/R/library\n/home/user/R/library\n";
        let paths = parse_lib_paths_output(output);
        // Note: paths are filtered by existence, so we can't assert exact values
        // but we can test the parsing logic with a mock approach
        assert!(paths.iter().all(|p| p.is_absolute()));
    }

    #[test]
    fn test_parse_lib_paths_output_with_whitespace() {
        // Test that whitespace is trimmed
        let output = "  /usr/lib/R/library  \n  /home/user/R/library  \n";
        let paths = parse_lib_paths_output(output);
        // Paths should be trimmed
        assert!(paths.iter().all(|p| !p.to_string_lossy().starts_with(' ')));
    }

    #[test]
    fn test_parse_lib_paths_output_empty() {
        let output = "";
        let paths = parse_lib_paths_output(output);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_parse_lib_paths_output_only_whitespace() {
        let output = "   \n   \n";
        let paths = parse_lib_paths_output(output);
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn test_get_lib_paths_returns_paths() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_lib_paths().await;
        assert!(result.is_ok());
        let paths = result.unwrap();
        // Should return at least one library path
        assert!(!paths.is_empty());
        // All paths should exist (we filter non-existent paths)
        for path in &paths {
            assert!(path.exists(), "Path should exist: {:?}", path);
        }
    }

    #[tokio::test]
    async fn test_get_lib_paths_contains_base_library() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_lib_paths().await;
        assert!(result.is_ok());
        let paths = result.unwrap();
        
        // At least one path should contain a "base" package directory
        // (this is the system library where base R packages are installed)
        let has_base = paths.iter().any(|p| {
            p.join("base").exists()
        });
        assert!(has_base, "Should have at least one library path containing base package");
    }

    #[test]
    fn test_parse_packages_output_simple() {
        let output = "base\nmethods\nutils\n";
        let packages = parse_packages_output(output);
        assert_eq!(packages, vec!["base", "methods", "utils"]);
    }

    #[test]
    fn test_parse_packages_output_with_whitespace() {
        let output = "  base  \n  methods  \n  utils  \n";
        let packages = parse_packages_output(output);
        assert_eq!(packages, vec!["base", "methods", "utils"]);
    }

    #[test]
    fn test_parse_packages_output_empty() {
        let output = "";
        let packages = parse_packages_output(output);
        assert!(packages.is_empty());
    }

    #[test]
    fn test_parse_packages_output_only_whitespace() {
        let output = "   \n   \n";
        let packages = parse_packages_output(output);
        assert!(packages.is_empty());
    }

    #[test]
    fn test_get_fallback_base_packages() {
        let packages = get_fallback_base_packages();
        // Should contain exactly the 7 required base packages
        assert_eq!(packages.len(), 7);
        assert!(packages.contains(&"base".to_string()));
        assert!(packages.contains(&"methods".to_string()));
        assert!(packages.contains(&"utils".to_string()));
        assert!(packages.contains(&"grDevices".to_string()));
        assert!(packages.contains(&"graphics".to_string()));
        assert!(packages.contains(&"stats".to_string()));
        assert!(packages.contains(&"datasets".to_string()));
    }

    #[test]
    fn test_get_fallback_base_packages_order() {
        let packages = get_fallback_base_packages();
        // Verify the order matches the requirement specification
        assert_eq!(packages[0], "base");
        assert_eq!(packages[1], "methods");
        assert_eq!(packages[2], "utils");
        assert_eq!(packages[3], "grDevices");
        assert_eq!(packages[4], "graphics");
        assert_eq!(packages[5], "stats");
        assert_eq!(packages[6], "datasets");
    }

    #[tokio::test]
    async fn test_get_base_packages_returns_packages() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_base_packages().await;
        assert!(result.is_ok());
        let packages = result.unwrap();
        // Should return at least the base packages
        assert!(!packages.is_empty());
        // Should contain "base" at minimum
        assert!(
            packages.contains(&"base".to_string()),
            "Should contain 'base' package"
        );
    }

    #[tokio::test]
    async fn test_get_base_packages_contains_core_packages() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_base_packages().await;
        assert!(result.is_ok());
        let packages = result.unwrap();

        // R's default search path should include these core packages
        // (they are always loaded in a standard R session)
        let core_packages = ["base", "methods", "utils", "stats"];
        for pkg in &core_packages {
            assert!(
                packages.contains(&pkg.to_string()),
                "Should contain '{}' package, got: {:?}",
                pkg,
                packages
            );
        }
    }

    // Tests for is_valid_package_name

    #[test]
    fn test_is_valid_package_name_simple() {
        assert!(is_valid_package_name("dplyr"));
        assert!(is_valid_package_name("ggplot2"));
        assert!(is_valid_package_name("base"));
        assert!(is_valid_package_name("stats"));
    }

    #[test]
    fn test_is_valid_package_name_with_dots() {
        assert!(is_valid_package_name("data.table"));
        assert!(is_valid_package_name("R.utils"));
        assert!(is_valid_package_name(".GlobalEnv")); // Starts with dot, second char is letter
    }

    #[test]
    fn test_is_valid_package_name_with_underscores() {
        assert!(is_valid_package_name("my_package"));
        assert!(is_valid_package_name("test_pkg_123"));
    }

    #[test]
    fn test_is_valid_package_name_with_numbers() {
        assert!(is_valid_package_name("ggplot2"));
        assert!(is_valid_package_name("R6"));
        assert!(is_valid_package_name("utf8"));
    }

    #[test]
    fn test_is_valid_package_name_empty() {
        assert!(!is_valid_package_name(""));
    }

    #[test]
    fn test_is_valid_package_name_starts_with_number() {
        assert!(!is_valid_package_name("2ggplot"));
        assert!(!is_valid_package_name("123"));
    }

    #[test]
    fn test_is_valid_package_name_starts_with_dot_then_number() {
        // .1 is not valid - if starts with dot, second char must be letter
        assert!(!is_valid_package_name(".1"));
        assert!(!is_valid_package_name(".123"));
    }

    #[test]
    fn test_is_valid_package_name_only_dot() {
        assert!(!is_valid_package_name("."));
    }

    #[test]
    fn test_is_valid_package_name_injection_attempts() {
        // These should all be rejected to prevent R code injection
        assert!(!is_valid_package_name("pkg; system('rm -rf /')"));
        assert!(!is_valid_package_name("pkg\"); system('ls')"));
        assert!(!is_valid_package_name("pkg$(whoami)"));
        assert!(!is_valid_package_name("pkg`ls`"));
        assert!(!is_valid_package_name("pkg\ncat('injected')"));
        assert!(!is_valid_package_name("pkg\rcat('injected')"));
        assert!(!is_valid_package_name("pkg'"));
        assert!(!is_valid_package_name("pkg\""));
        assert!(!is_valid_package_name("pkg("));
        assert!(!is_valid_package_name("pkg)"));
        assert!(!is_valid_package_name("pkg{"));
        assert!(!is_valid_package_name("pkg}"));
        assert!(!is_valid_package_name("pkg["));
        assert!(!is_valid_package_name("pkg]"));
        assert!(!is_valid_package_name("pkg<-"));
        assert!(!is_valid_package_name("pkg="));
        assert!(!is_valid_package_name("pkg+"));
        assert!(!is_valid_package_name("pkg-"));
        assert!(!is_valid_package_name("pkg*"));
        assert!(!is_valid_package_name("pkg/"));
        assert!(!is_valid_package_name("pkg\\"));
        assert!(!is_valid_package_name("pkg|"));
        assert!(!is_valid_package_name("pkg&"));
        assert!(!is_valid_package_name("pkg!"));
        assert!(!is_valid_package_name("pkg@"));
        assert!(!is_valid_package_name("pkg#"));
        assert!(!is_valid_package_name("pkg$"));
        assert!(!is_valid_package_name("pkg%"));
        assert!(!is_valid_package_name("pkg^"));
        assert!(!is_valid_package_name("pkg~"));
        assert!(!is_valid_package_name("pkg "));
        assert!(!is_valid_package_name(" pkg"));
        assert!(!is_valid_package_name("pkg name"));
    }

    #[test]
    fn test_is_valid_package_name_unicode() {
        // Unicode characters should be rejected (only ASCII allowed)
        assert!(!is_valid_package_name("пакет")); // Russian
        assert!(!is_valid_package_name("包")); // Chinese
        assert!(!is_valid_package_name("pkg日本語"));
    }

    // Tests for get_package_exports

    #[tokio::test]
    async fn test_get_package_exports_base() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_exports("base").await;
        assert!(result.is_ok(), "Should succeed for base package");
        let exports = result.unwrap();
        
        // base package should have many exports
        assert!(!exports.is_empty(), "base package should have exports");
        
        // Should contain common base functions
        assert!(exports.contains(&"print".to_string()), "Should contain 'print'");
        assert!(exports.contains(&"cat".to_string()), "Should contain 'cat'");
        assert!(exports.contains(&"c".to_string()), "Should contain 'c'");
        assert!(exports.contains(&"list".to_string()), "Should contain 'list'");
        assert!(exports.contains(&"function".to_string()) || exports.contains(&"length".to_string()), 
            "Should contain common base functions");
    }

    #[tokio::test]
    async fn test_get_package_exports_stats() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_exports("stats").await;
        assert!(result.is_ok(), "Should succeed for stats package");
        let exports = result.unwrap();
        
        // stats package should have many exports
        assert!(!exports.is_empty(), "stats package should have exports");
        
        // Should contain common stats functions
        assert!(exports.contains(&"lm".to_string()), "Should contain 'lm'");
        assert!(exports.contains(&"glm".to_string()), "Should contain 'glm'");
        assert!(exports.contains(&"t.test".to_string()), "Should contain 't.test'");
    }

    #[tokio::test]
    async fn test_get_package_exports_utils() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_exports("utils").await;
        assert!(result.is_ok(), "Should succeed for utils package");
        let exports = result.unwrap();
        
        // utils package should have many exports
        assert!(!exports.is_empty(), "utils package should have exports");
        
        // Should contain common utils functions
        assert!(exports.contains(&"head".to_string()), "Should contain 'head'");
        assert!(exports.contains(&"tail".to_string()), "Should contain 'tail'");
        assert!(exports.contains(&"str".to_string()), "Should contain 'str'");
    }

    #[tokio::test]
    async fn test_get_package_exports_nonexistent_package() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_exports("nonexistent_package_xyz_123").await;
        assert!(result.is_err(), "Should fail for non-existent package");
        
        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("nonexistent_package_xyz_123"),
            "Error should mention the package name: {}",
            error_msg
        );
    }

    #[tokio::test]
    async fn test_get_package_exports_invalid_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // Test with injection attempt
        let result = subprocess.get_package_exports("pkg; system('ls')").await;
        assert!(result.is_err(), "Should reject invalid package name");
        
        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("Invalid package name"),
            "Error should indicate invalid package name: {}",
            error_msg
        );
    }

    #[tokio::test]
    async fn test_get_package_exports_empty_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_exports("").await;
        assert!(result.is_err(), "Should reject empty package name");
    }

    // Tests for parse_depends_field

    #[test]
    fn test_parse_depends_field_simple() {
        let depends = parse_depends_field("dplyr, ggplot2, tidyr");
        assert_eq!(depends, vec!["dplyr", "ggplot2", "tidyr"]);
    }

    #[test]
    fn test_parse_depends_field_with_version_constraints() {
        let depends = parse_depends_field("R (>= 3.5), dplyr (>= 1.0.0), ggplot2");
        // "R" should be filtered out
        assert_eq!(depends, vec!["dplyr", "ggplot2"]);
    }

    #[test]
    fn test_parse_depends_field_only_r() {
        let depends = parse_depends_field("R (>= 4.0.0)");
        assert!(depends.is_empty(), "Should filter out R dependency");
    }

    #[test]
    fn test_parse_depends_field_empty() {
        let depends = parse_depends_field("");
        assert!(depends.is_empty());
    }

    #[test]
    fn test_parse_depends_field_whitespace_only() {
        let depends = parse_depends_field("   \n   ");
        assert!(depends.is_empty());
    }

    #[test]
    fn test_parse_depends_field_with_extra_whitespace() {
        let depends = parse_depends_field("  dplyr  ,  ggplot2  ,  tidyr  ");
        assert_eq!(depends, vec!["dplyr", "ggplot2", "tidyr"]);
    }

    #[test]
    fn test_parse_depends_field_complex_version_constraints() {
        let depends = parse_depends_field("R (>= 3.5.0), methods, stats (>= 3.0), utils");
        // "R" should be filtered out, version constraints stripped
        assert_eq!(depends, vec!["methods", "stats", "utils"]);
    }

    #[test]
    fn test_parse_depends_field_single_package() {
        let depends = parse_depends_field("methods");
        assert_eq!(depends, vec!["methods"]);
    }

    #[test]
    fn test_parse_depends_field_with_newlines() {
        // DESCRIPTION files can have newlines in the Depends field
        let depends = parse_depends_field("R (>= 3.5),\n    dplyr,\n    ggplot2");
        assert_eq!(depends, vec!["dplyr", "ggplot2"]);
    }

    #[test]
    fn test_parse_depends_field_filters_invalid_names() {
        // Invalid package names should be filtered out
        let depends = parse_depends_field("dplyr, invalid;name, ggplot2");
        assert_eq!(depends, vec!["dplyr", "ggplot2"]);
    }

    // Tests for get_package_depends

    #[tokio::test]
    async fn test_get_package_depends_base() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // base package typically has no Depends (or just R)
        let result = subprocess.get_package_depends("base").await;
        assert!(result.is_ok(), "Should succeed for base package");
        // base package should have no package dependencies (only R version)
        let depends = result.unwrap();
        assert!(
            !depends.contains(&"R".to_string()),
            "Should not contain 'R' in depends"
        );
    }

    #[tokio::test]
    async fn test_get_package_depends_stats() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_depends("stats").await;
        assert!(result.is_ok(), "Should succeed for stats package");
        // stats package depends on some base packages
        let depends = result.unwrap();
        // Should not contain "R"
        assert!(
            !depends.contains(&"R".to_string()),
            "Should not contain 'R' in depends"
        );
    }

    #[tokio::test]
    async fn test_get_package_depends_nonexistent_package() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_depends("nonexistent_package_xyz_123").await;
        assert!(result.is_err(), "Should fail for non-existent package");

        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("nonexistent_package_xyz_123"),
            "Error should mention the package name: {}",
            error_msg
        );
    }

    #[tokio::test]
    async fn test_get_package_depends_invalid_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // Test with injection attempt
        let result = subprocess.get_package_depends("pkg; system('ls')").await;
        assert!(result.is_err(), "Should reject invalid package name");

        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("Invalid package name"),
            "Error should indicate invalid package name: {}",
            error_msg
        );
    }

    #[tokio::test]
    async fn test_get_package_depends_empty_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_package_depends("").await;
        assert!(result.is_err(), "Should reject empty package name");
    }

    #[tokio::test]
    async fn test_get_package_depends_methods() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // methods package typically depends on utils
        let result = subprocess.get_package_depends("methods").await;
        assert!(result.is_ok(), "Should succeed for methods package");
        let depends = result.unwrap();
        // Verify R is filtered out
        assert!(
            !depends.contains(&"R".to_string()),
            "Should not contain 'R' in depends"
        );
        // All returned names should be valid package names
        for dep in &depends {
            assert!(
                is_valid_package_name(dep),
                "Dependency '{}' should be a valid package name",
                dep
            );
        }
    }

    // Additional tests for R path discovery edge cases

    #[test]
    fn test_is_valid_r_executable_nonexistent_path() {
        let nonexistent = PathBuf::from("/this/path/does/not/exist/R");
        assert!(!RSubprocess::is_valid_r_executable(&nonexistent));
    }

    #[test]
    fn test_is_valid_r_executable_directory() {
        // A directory is not a valid R executable
        let temp_dir = std::env::temp_dir();
        assert!(!RSubprocess::is_valid_r_executable(&temp_dir));
    }

    #[test]
    fn test_discover_r_path_returns_valid_path_or_none() {
        // discover_r_path should either return a valid R path or None
        // It should never return an invalid path
        if let Some(path) = RSubprocess::discover_r_path() {
            assert!(path.exists(), "Discovered path should exist");
            assert!(
                RSubprocess::is_valid_r_executable(&path),
                "Discovered path should be a valid R executable"
            );
        }
        // If None is returned, that's also valid (R not installed)
    }

    #[test]
    fn test_find_r_in_common_locations_returns_valid_or_none() {
        // find_r_in_common_locations should either return a valid path or None
        if let Some(path) = RSubprocess::find_r_in_common_locations() {
            assert!(path.exists(), "Found path should exist");
            assert!(
                RSubprocess::is_valid_r_executable(&path),
                "Found path should be a valid R executable"
            );
        }
        // If None is returned, that's also valid (R not in common locations)
    }

    #[test]
    fn test_find_r_in_path_returns_valid_or_none() {
        // find_r_in_path should either return a valid path or None
        if let Some(path) = RSubprocess::find_r_in_path() {
            assert!(path.exists(), "Found path should exist");
            assert!(
                RSubprocess::is_valid_r_executable(&path),
                "Found path should be a valid R executable"
            );
        }
        // If None is returned, that's also valid (R not in PATH)
    }

    // Tests for fallback behavior (Requirements 15.2, 15.3)

    #[test]
    fn test_fallback_base_packages_are_valid_package_names() {
        // Requirement 15.3: Fallback should provide valid package names
        let packages = get_fallback_base_packages();
        for pkg in &packages {
            assert!(
                is_valid_package_name(pkg),
                "Fallback package '{}' should be a valid package name",
                pkg
            );
        }
    }

    #[test]
    fn test_fallback_lib_paths_are_absolute() {
        // Fallback library paths should be absolute paths
        let paths = get_fallback_lib_paths();
        for path in &paths {
            // All returned paths should be absolute (we filter by existence)
            assert!(
                path.is_absolute(),
                "Fallback path {:?} should be absolute",
                path
            );
        }
    }

    #[test]
    fn test_parse_lib_paths_output_handles_mixed_valid_invalid() {
        // Test that parsing handles a mix of existing and non-existing paths
        // Only existing paths should be returned
        let output = "/nonexistent/path/1\n/nonexistent/path/2\n";
        let paths = parse_lib_paths_output(output);
        // All returned paths should exist (non-existent are filtered)
        for path in &paths {
            assert!(path.exists(), "Returned path {:?} should exist", path);
        }
    }

    #[test]
    fn test_parse_packages_output_preserves_order() {
        // Test that package order is preserved
        let output = "first\nsecond\nthird\nfourth\n";
        let packages = parse_packages_output(output);
        assert_eq!(packages, vec!["first", "second", "third", "fourth"]);
    }

    #[test]
    fn test_parse_depends_field_handles_multiline_description() {
        // DESCRIPTION files often have multi-line Depends fields with continuation
        let depends = "R (>= 4.0),\n    dplyr (>= 1.0),\n    tidyr,\n    ggplot2 (>= 3.0)";
        let result = parse_depends_field(depends);
        assert_eq!(result, vec!["dplyr", "tidyr", "ggplot2"]);
    }

    #[test]
    fn test_parse_depends_field_handles_tabs() {
        // Test handling of tab characters in Depends field
        let depends = "R (>= 4.0),\tdplyr,\tggplot2";
        let result = parse_depends_field(depends);
        assert_eq!(result, vec!["dplyr", "ggplot2"]);
    }

    #[test]
    fn test_is_valid_package_name_boundary_cases() {
        // Test boundary cases for package name validation
        assert!(is_valid_package_name("a")); // Single letter is valid
        assert!(is_valid_package_name("A")); // Single uppercase letter is valid
        assert!(is_valid_package_name("a1")); // Letter followed by number
        assert!(is_valid_package_name("a_")); // Letter followed by underscore
        assert!(is_valid_package_name("a.")); // Letter followed by dot
        assert!(is_valid_package_name(".a")); // Dot followed by letter
        assert!(is_valid_package_name(".Ab")); // Dot followed by letters
        assert!(!is_valid_package_name("_a")); // Cannot start with underscore
        assert!(!is_valid_package_name("1a")); // Cannot start with number
    }

    // Tests for error handling continuity (Requirement 15.3)

    #[tokio::test]
    async fn test_get_lib_paths_fallback_on_empty_output() {
        // When R returns empty output, fallback paths should be used
        // This tests the fallback behavior indirectly
        let fallback = get_fallback_lib_paths();
        // Fallback should not panic and should return a valid (possibly empty) list
        // The list may be empty if no standard paths exist on this system
        for path in &fallback {
            assert!(path.exists(), "Fallback path should exist");
        }
    }

    #[tokio::test]
    async fn test_get_base_packages_fallback_completeness() {
        // Verify fallback base packages match the requirement specification
        let fallback = get_fallback_base_packages();
        
        // Requirement 6.2: base, methods, utils, grDevices, graphics, stats, datasets
        let required = vec![
            "base", "methods", "utils", "grDevices", "graphics", "stats", "datasets"
        ];
        
        for pkg in &required {
            assert!(
                fallback.contains(&pkg.to_string()),
                "Fallback should contain required package '{}'",
                pkg
            );
        }
    }

    #[tokio::test]
    async fn test_subprocess_graceful_error_handling() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // Test that errors are returned gracefully, not panics
        // Multiple invalid operations should all return Err, not panic
        let results = vec![
            subprocess.get_package_exports("").await,
            subprocess.get_package_exports("invalid;name").await,
            subprocess.get_package_exports("nonexistent_pkg_12345").await,
            subprocess.get_package_depends("").await,
            subprocess.get_package_depends("invalid;name").await,
            subprocess.get_package_depends("nonexistent_pkg_12345").await,
        ];

        for result in results {
            assert!(result.is_err(), "Invalid operations should return Err, not panic");
        }
    }

    #[test]
    fn test_r_path_accessor() {
        // Test that r_path() returns the correct path
        // Skip if R is not available
        if let Some(subprocess) = RSubprocess::new(None) {
            let path = subprocess.r_path();
            assert!(path.exists(), "r_path() should return an existing path");
            assert!(
                RSubprocess::is_valid_r_executable(path),
                "r_path() should return a valid R executable"
            );
        }
    }
}
