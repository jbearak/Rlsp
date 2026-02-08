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
    /// Working directory for R subprocess
    working_dir: Option<PathBuf>,
}

impl RSubprocess {
    /// Creates a configured RSubprocess when an R executable path can be validated or discovered.
    ///
    /// If `r_path` is `Some(path)`, the provided path is validated as an R executable and used on success.
    /// If `r_path` is `None`, the function attempts to discover an R executable in the environment or common locations.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// // If an explicit invalid path is given, `new` returns `None`.
    /// assert!(crate::r_subprocess::RSubprocess::new(Some(PathBuf::from("/no/such/path"))).is_none());
    /// // When no path is provided, `new` attempts discovery and may return `Some` or `None` depending on the environment.
    /// let _ = crate::r_subprocess::RSubprocess::new(None);
    /// ```
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
            Self {
                r_path,
                working_dir: None,
            }
        })
    }

    /// Set the working directory for the R subprocess
    pub fn with_working_dir(mut self, path: PathBuf) -> Self {
        self.working_dir = Some(path);
        self
    }

    /// Get the path to the R executable
    pub fn r_path(&self) -> &PathBuf {
        &self.r_path
    }

    /// Locate an R executable on the system by searching common locations.
    ///
    /// Attempts to find an R binary first via the system PATH and then by checking
    /// a set of typical installation locations for the current platform.
    ///
    /// # Returns
    ///
    /// `Some(PathBuf)` containing the path to an R executable if found, `None` if no candidate was discovered.
    ///
    /// # Examples
    ///
    /// ```
    /// if let Some(r_path) = discover_r_path() {
    ///     println!("Found R at {:?}", r_path);
    /// } else {
    ///     println!("R not found on this system");
    /// }
    /// ```
    fn discover_r_path() -> Option<PathBuf> {
        // First, try to find R in PATH using `which` on Unix or `where` on Windows
        if let Some(path) = Self::find_r_in_path() {
            return Some(path);
        }

        // Fall back to common installation locations
        Self::find_r_in_common_locations()
    }

    /// Locate an R executable by searching the system PATH.
    ///
    /// Returns `Some(PathBuf)` with the first valid R executable found in PATH, or `None` if no valid executable is discovered.
    /// The function validates any candidate before returning it.
    ///
    /// # Examples
    ///
    /// ```
    /// // Returns `Some` when an R executable is available on PATH, otherwise `None`.
    /// if let Some(path) = find_r_in_path() {
    ///     println!("Found R at: {}", path.display());
    /// }
    /// ```
    fn find_r_in_path() -> Option<PathBuf> {
        #[cfg(unix)]
        {
            let output = std::process::Command::new("which").arg("R").output().ok()?;

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
            let output = std::process::Command::new("where").arg("R").output().ok()?;

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

    /// Searches common installation locations for an R executable and returns the first valid candidate.
    ///
    /// # Examples
    ///
    /// ```
    /// // Usage: returns Some(path) if a known R installation is found, or None otherwise.
    /// let _ = find_r_in_common_locations();
    /// ```
    fn find_r_in_common_locations() -> Option<PathBuf> {
        let common_paths = Self::get_common_r_paths();
        common_paths.into_iter().find(Self::is_valid_r_executable)
    }

    /// Lists common filesystem locations where an R executable is typically installed for the current target OS.
    ///
    /// The returned list contains platform-specific candidate paths (macOS, Linux, Windows) in a preferred order.
    /// Entries are suggestions and may point to non-existent files; callers should validate existence before use.
    ///
    /// # Examples
    ///
    /// ```
    /// let paths = get_common_r_paths();
    /// // Paths are returned as absolute filesystem locations (may or may not exist)
    /// assert!(paths.iter().all(|p| p.is_absolute()));
    /// ```
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

    /// Determines whether the given path points to a working R executable.
    ///
    /// Checks that the file exists and that invoking it with `--version` either
    /// returns a successful exit status or prints an R version string to stderr.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// let candidate = PathBuf::from("/usr/bin/R");
    /// // May be `true` on systems with R installed, `false` otherwise.
    /// let _ok = is_valid_r_executable(&candidate);
    /// ```
    ///
    /// # Returns
    ///
    /// `true` if the path exists and appears to be an R executable, `false` otherwise.
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

    /// Executes an R expression using the configured R executable and returns its stdout output.
    ///
    /// # Errors
    ///
    /// Returns an error if the R subprocess cannot be spawned or if R exits with a non-zero status,
    /// in which case the error contains the process status and stderr content.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// // Construct an RSubprocess pointing to an `R` executable on PATH or a full path.
    /// let r = RSubprocess::new(Some(PathBuf::from("R"))).unwrap();
    /// let rt = tokio::runtime::Runtime::new().unwrap();
    /// let out = rt.block_on(r.execute_r_code(r#"cat("ok")"#)).unwrap();
    /// assert_eq!(out.trim(), "ok");
    /// ```
    /// Default timeout for R subprocess calls (30 seconds).
    const SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    pub async fn execute_r_code(&self, r_code: &str) -> Result<String> {
        self.execute_r_code_with_timeout(r_code, Self::SUBPROCESS_TIMEOUT)
            .await
    }

    /// Execute R code with a configurable timeout.
    ///
    /// Returns an error if the subprocess does not complete within the given
    /// duration. This prevents hung R processes from blocking the LSP
    /// indefinitely (e.g. during initialization or package queries).
    pub async fn execute_r_code_with_timeout(
        &self,
        r_code: &str,
        timeout: std::time::Duration,
    ) -> Result<String> {
        let start = std::time::Instant::now();
        crate::perf::increment_r_subprocess_calls();

        let mut cmd = Command::new(&self.r_path);
        cmd.args(["--vanilla", "--slave", "-e", r_code]);

        if let Some(wd) = &self.working_dir {
            cmd.current_dir(wd);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn R subprocess: {e}"))?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result.map_err(|e| anyhow!("Failed to execute R subprocess: {e}"))?,
            Err(_) => {
                return Err(anyhow!("R subprocess timed out after {timeout:?}"));
            }
        };

        let elapsed = start.elapsed();
        if crate::perf::is_enabled() {
            // Truncate r_code for logging (first 50 chars)
            let code_preview: String = r_code.chars().take(50).collect();
            let code_preview = if r_code.len() > 50 {
                format!("{}...", code_preview)
            } else {
                code_preview
            };
            log::info!("[PERF] R subprocess call ({:?}): {}", elapsed, code_preview);
        }

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

    /// Returns the R library search paths known to the R installation or a platform-specific fallback.
    ///
    /// Attempts to obtain library paths by invoking R's `.libPaths()` and parsing each path from the process output. If invoking R fails or yields no valid paths, returns a platform-specific list of common R library locations.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<PathBuf>)` containing the resolved library directories; when R cannot be queried or returns no paths, the vector contains fallback platform-standard library paths.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::path::PathBuf;
    /// # use rlsp::r_subprocess::RSubprocess;
    /// # async fn doc_example() {
    /// let rsub = RSubprocess::new(None).expect("R executable not found");
    /// let lib_paths = rsub.get_lib_paths().await.unwrap();
    /// assert!(!lib_paths.is_empty());
    /// # }
    /// ```
    pub async fn get_lib_paths(&self) -> Result<Vec<PathBuf>> {
        // Use cat() with sep="\n" to output each path on its own line without R's vector formatting
        // Check for renv/activate.R and source it if it exists (handles renv projects)
        // Security: Validate that renv/activate.R is in the working directory to prevent path traversal
        let r_code = r#"renv_path <- normalizePath("renv/activate.R", mustWork=FALSE); if (file.exists(renv_path) && dirname(renv_path) == file.path(getwd(), "renv")) try(source(renv_path), silent=TRUE); cat(.libPaths(), sep="\n")"#;

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
                log::trace!(
                    "Failed to get .libPaths() from R: {}, using fallback paths",
                    e
                );
                Ok(get_fallback_lib_paths())
            }
        }
    }

    /// Retrieve the base (startup) packages provided by the R installation.
    ///
    /// Queries R for `.packages()` and returns the resulting package names. If the
    /// R subprocess is unavailable or returns an empty result, returns a stable
    /// fallback list: `["base", "methods", "utils", "grDevices", "graphics", "stats", "datasets"]`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Run the async method using a small runtime for demonstration.
    /// // Replace the constructor below with the actual path-discovery call used in your project.
    /// # use std::path::PathBuf;
    /// # use tokio::runtime::Runtime;
    /// # use crates::rlsp::r_subprocess::RSubprocess;
    /// let r = RSubprocess::new(None).expect("R executable not found");
    /// let rt = Runtime::new().unwrap();
    /// let pkgs = rt.block_on(async { r.get_base_packages().await.unwrap() });
    /// assert!(pkgs.contains(&"base".to_string()));
    /// ```
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

    /// Retrieve the exported symbol names of an installed R package.
    ///
    /// The package name is validated to prevent R code injection; only ASCII letters,
    /// digits, dots, and underscores are allowed, and names must start with a letter
    /// or a dot (if starting with a dot, the second character must be a letter).
    /// Returns an error if the name is invalid, the package is not installed, or
    /// the R subprocess fails.
    ///
    /// # Parameters
    ///
    /// - `package` — Name of the package whose exports to retrieve. See validation rules above.
    ///
    /// # Returns
    ///
    /// A vector of exported symbol names from the package.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Runs the async call on the current thread
    /// let rp = RSubprocess::new(None).expect("R executable not found");
    /// let exports = tokio::runtime::Runtime::new()
    ///     .unwrap()
    ///     .block_on(rp.get_package_exports("stats"))
    ///     .expect("failed to get exports");
    /// assert!(exports.iter().any(|s| s == "lm"));
    /// ```
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
            return Err(anyhow!(
                "Failed to get exports for package '{}': {}",
                package,
                error_msg
            ));
        }

        // Parse the output - one export name per line
        let exports = parse_packages_output(&output);
        if exports.is_empty() {
            let preview = if output.len() > 200 {
                // Safe UTF-8 truncation: find the last valid char boundary before 200 bytes
                let truncate_at = output
                    .char_indices()
                    .take_while(|(i, _)| *i < 200)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("{}...", &output[..truncate_at])
            } else {
                output.clone()
            };
            log::trace!(
                "R returned empty exports for package '{}'; stdout_len={}, stdout_preview={:?}",
                package,
                output.len(),
                preview
            );
        }

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

    /// Retrieve the list of package names declared in a package's DESCRIPTION `Depends` field.
    ///
    /// The returned list contains only package names: version constraints (e.g., `(>= 3.5)`) are
    /// removed and the special `R` entry is filtered out. Package names are validated to prevent
    /// injection; if validation fails, or if R cannot read the DESCRIPTION (for example the package
    /// is not installed or the R subprocess fails), an error is returned.
    ///
    /// # Returns
    ///
    /// `Ok(Vec<String>)` with the cleaned dependency package names on success, `Err` if the package
    /// is not installed, the package name is invalid, or the R subprocess fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use rlsp::r_subprocess::RSubprocess;
    /// # tokio_test::block_on(async {
    /// if let Some(r) = RSubprocess::new(None) {
    ///     // Retrieves dependencies declared in the DESCRIPTION of the "stats" package.
    ///     let deps = r.get_package_depends("stats").await.unwrap();
    ///     assert!(deps.iter().all(|name| !name.is_empty()));
    /// }
    /// # });
    /// ```
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

    /// Retrieve exports for multiple packages in a single R subprocess call.
    ///
    /// This is significantly faster than calling `get_package_exports` multiple times,
    /// as it eliminates the overhead of spawning multiple R processes (~75-350ms each).
    ///
    /// # Parameters
    ///
    /// - `packages` — List of package names whose exports to retrieve
    ///
    /// # Returns
    ///
    /// A HashMap mapping package name to its exports. Packages that couldn't be loaded
    /// (not installed, errors) will have empty export lists.
    ///
    /// # Performance
    ///
    /// This method replaces N R subprocess calls with a single call, saving
    /// approximately (N-1) * 75-350ms on typical systems.
    pub async fn get_multiple_package_exports(
        &self,
        packages: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<String>>> {
        if packages.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Validate all package names
        for pkg in packages {
            if !is_valid_package_name(pkg) {
                return Err(anyhow!(
                    "Invalid package name '{}': must contain only letters, numbers, dots, and underscores",
                    pkg
                ));
            }
        }

        // Build R code that queries all packages
        let packages_vector = packages
            .iter()
            .map(|p| format!("\"{}\"", p))
            .collect::<Vec<_>>()
            .join(", ");

        let r_code = format!(
            r#"
pkgs <- c({})
cat("__RAVEN_MULTI_EXPORTS__\n")
for (pkg in pkgs) {{
    cat(paste0("__PKG:", pkg, "__\n"))
    tryCatch(
        cat(getNamespaceExports(asNamespace(pkg)), sep="\n"),
        error = function(e) {{}}
    )
}}
cat("__RAVEN_END__\n")
"#,
            packages_vector
        );

        let output = self.execute_r_code(&r_code).await?;

        // Parse the structured output
        parse_multi_exports_output(&output)
    }

    /// Batch initialization: retrieve lib_paths, base_packages, and all base package exports
    /// in a single R subprocess call.
    ///
    /// This is significantly faster than making separate calls for each piece of data,
    /// as it eliminates the overhead of spawning multiple R processes (~100-300ms each).
    ///
    /// # Returns
    ///
    /// A `BatchInitResult` containing:
    /// - `lib_paths`: Library paths from `.libPaths()`
    /// - `base_packages`: Base packages from `.packages()`
    /// - `base_exports`: Combined exports from all base packages
    ///
    /// # Performance
    ///
    /// This method replaces 2 + N R subprocess calls (where N is the number of base packages,
    /// typically 7) with a single call, saving approximately 700-2100ms on typical systems.
    pub async fn initialize_batch(&self) -> Result<BatchInitResult> {
        // Single R script that outputs all needed data in a structured format
        // We use markers to separate sections for reliable parsing
        let r_code = r#"
# Handle renv activation for project-local libraries
renv_path <- normalizePath("renv/activate.R", mustWork=FALSE)
if (file.exists(renv_path) && dirname(renv_path) == file.path(getwd(), "renv")) {
    try(source(renv_path), silent=TRUE)
}

# Output library paths
cat("__RAVEN_LIB_PATHS__\n")
cat(.libPaths(), sep="\n")

# Output base packages
cat("\n__RAVEN_BASE_PACKAGES__\n")
pkgs <- .packages()
cat(pkgs, sep="\n")

# Output exports for each base package
cat("\n__RAVEN_EXPORTS__\n")
for (pkg in pkgs) {
    cat(paste0("__PKG:", pkg, "__\n"))
    tryCatch(
        cat(getNamespaceExports(asNamespace(pkg)), sep="\n"),
        error = function(e) {}
    )
}
cat("__RAVEN_END__\n")
"#;

        let output = self.execute_r_code(r_code).await?;

        // Parse the structured output
        parse_batch_init_output(&output)
    }
}

/// Result of batch initialization from R subprocess
#[derive(Debug, Clone, Default)]
pub struct BatchInitResult {
    /// Library paths from `.libPaths()`
    pub lib_paths: Vec<PathBuf>,
    /// Base packages from `.packages()`
    pub base_packages: Vec<String>,
    /// Exports for each base package (package name -> list of exports)
    pub package_exports: std::collections::HashMap<String, Vec<String>>,
}

impl BatchInitResult {
    /// Get combined exports from all base packages
    pub fn all_base_exports(&self) -> std::collections::HashSet<String> {
        self.package_exports
            .values()
            .flat_map(|exports| exports.iter().cloned())
            .collect()
    }
}

/// Parse the output of `initialize_batch()` into a `BatchInitResult`
fn parse_batch_init_output(output: &str) -> Result<BatchInitResult> {
    let mut result = BatchInitResult::default();

    // Split by section markers
    let lib_paths_start = output
        .find("__RAVEN_LIB_PATHS__")
        .ok_or_else(|| anyhow!("Missing __RAVEN_LIB_PATHS__ marker in R output"))?;
    let base_packages_start = output
        .find("__RAVEN_BASE_PACKAGES__")
        .ok_or_else(|| anyhow!("Missing __RAVEN_BASE_PACKAGES__ marker in R output"))?;
    let exports_start = output
        .find("__RAVEN_EXPORTS__")
        .ok_or_else(|| anyhow!("Missing __RAVEN_EXPORTS__ marker in R output"))?;

    // Parse lib_paths section
    let lib_paths_section =
        &output[lib_paths_start + "__RAVEN_LIB_PATHS__".len()..base_packages_start];
    result.lib_paths = lib_paths_section
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect();

    // Parse base_packages section
    let base_packages_section =
        &output[base_packages_start + "__RAVEN_BASE_PACKAGES__".len()..exports_start];
    result.base_packages = base_packages_section
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect();

    // Parse exports section - each package is marked with __PKG:name__
    let exports_section = &output[exports_start + "__RAVEN_EXPORTS__".len()..];
    let mut current_package: Option<String> = None;
    let mut current_exports: Vec<String> = Vec::new();

    for line in exports_section.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with("__PKG:") && line.ends_with("__") {
            // Save previous package exports
            if let Some(pkg) = current_package.take() {
                result
                    .package_exports
                    .insert(pkg, std::mem::take(&mut current_exports));
            }
            // Start new package
            let pkg_name = &line[6..line.len() - 2]; // Strip __PKG: and __
            current_package = Some(pkg_name.to_string());
        } else if line == "__RAVEN_END__" {
            // End marker - save final package
            if let Some(pkg) = current_package.take() {
                result
                    .package_exports
                    .insert(pkg, std::mem::take(&mut current_exports));
            }
            break;
        } else if current_package.is_some() {
            // Export name
            current_exports.push(line.to_string());
        }
    }

    // Handle case where __RAVEN_END__ was missing
    if let Some(pkg) = current_package {
        result
            .package_exports
            .insert(pkg, std::mem::take(&mut current_exports));
    }

    log::trace!(
        "Batch init: {} lib_paths, {} base_packages, {} packages with exports",
        result.lib_paths.len(),
        result.base_packages.len(),
        result.package_exports.len()
    );

    Ok(result)
}

/// Parse the output of `get_multiple_package_exports()` into a HashMap
fn parse_multi_exports_output(
    output: &str,
) -> Result<std::collections::HashMap<String, Vec<String>>> {
    let mut result = std::collections::HashMap::new();

    // Find the start marker
    let exports_start = output
        .find("__RAVEN_MULTI_EXPORTS__")
        .ok_or_else(|| anyhow!("Missing __RAVEN_MULTI_EXPORTS__ marker in R output"))?;

    // Parse exports section - each package is marked with __PKG:name__
    let exports_section = &output[exports_start + "__RAVEN_MULTI_EXPORTS__".len()..];
    let mut current_package: Option<String> = None;
    let mut current_exports: Vec<String> = Vec::new();

    for line in exports_section.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with("__PKG:") && line.ends_with("__") {
            // Save previous package exports
            if let Some(pkg) = current_package.take() {
                result.insert(pkg, std::mem::take(&mut current_exports));
            }
            // Start new package
            let pkg_name = &line[6..line.len() - 2]; // Strip __PKG: and __
            current_package = Some(pkg_name.to_string());
        } else if line == "__RAVEN_END__" {
            // End marker - save final package
            if let Some(pkg) = current_package.take() {
                result.insert(pkg, std::mem::take(&mut current_exports));
            }
            break;
        } else if current_package.is_some() {
            // Export name
            current_exports.push(line.to_string());
        }
    }

    // Handle case where __RAVEN_END__ was missing
    if let Some(pkg) = current_package {
        result.insert(pkg, std::mem::take(&mut current_exports));
    }

    log::trace!("Multi-export query: {} packages with exports", result.len());

    Ok(result)
}

/// Parse an R DESCRIPTION `Depends` field into its package names.
///
/// This returns a Vec of package names in the same order they appear in `depends_str`.
/// Each comma-separated entry is trimmed, any version constraint in parentheses is removed,
/// the special entry `"R"` is excluded, and remaining names are validated as package identifiers.
///
/// # Arguments
///
/// * `depends_str` - The raw Depends field value (e.g. `R (>= 3.5), dplyr, ggplot2`).
///
/// # Returns
///
/// A `Vec<String>` containing valid package names extracted from `depends_str`, or an empty
/// vector if there are no valid package names.
///
/// # Examples
///
/// ```
/// let v = parse_depends_field("R (>= 3.5), dplyr, ggplot2");
/// assert_eq!(v, vec!["dplyr".to_string(), "ggplot2".to_string()]);
/// ```
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

/// Parse newline-separated R library paths into a vector of existing `PathBuf`s.
///
/// Trims each line, ignores empty lines, converts each remaining line into a `PathBuf`,
/// and retains only paths that exist on the filesystem.
///
/// # Examples
///
/// ```
/// use std::fs;
/// use std::path::PathBuf;
/// let dir = std::env::temp_dir().join("r_libs_example");
/// let dir2 = dir.join("sub");
/// let _ = fs::create_dir_all(&dir2);
/// let input = format!("{}\n{}\n\n", dir.display(), dir2.display());
/// let paths = crate::parse_lib_paths_output(&input);
/// assert!(paths.contains(&PathBuf::from(dir)));
/// assert!(paths.contains(&PathBuf::from(dir2)));
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

/// Parse the output of R's `.packages()` (one package name per line) into a list of package names.
///
/// # Examples
///
/// ```
/// let out = "base\nmethods\nutils\n";
/// let pkgs = parse_packages_output(out);
/// assert_eq!(pkgs, vec!["base".to_string(), "methods".to_string(), "utils".to_string()]);
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

/// Hardcoded list of core R base packages in standard order.
///
/// This list is used as a fallback when an R subprocess is unavailable.
///
/// # Returns
///
/// A `Vec<String>` containing: `"base"`, `"methods"`, `"utils"`, `"grDevices"`, `"graphics"`, `"stats"`, `"datasets"`.
///
/// # Examples
///
/// ```
/// let pkgs = get_fallback_base_packages();
/// assert_eq!(pkgs, vec![
///     "base".to_string(),
///     "methods".to_string(),
///     "utils".to_string(),
///     "grDevices".to_string(),
///     "graphics".to_string(),
///     "stats".to_string(),
///     "datasets".to_string(),
/// ]);
/// ```
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

/// Platform-specific candidate R library directories used as a fallback when an R subprocess is unavailable.
///
/// This returns a curated list of common system, user, and package-manager library locations for macOS, Linux, and Windows,
/// filtered to only include paths that exist on the filesystem.
///
/// # Examples
///
/// ```
/// let paths = get_fallback_lib_paths();
/// for p in &paths {
///     // returned paths are absolute filesystem paths
///     assert!(p.is_absolute());
/// }
/// ```
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
            paths.push(PathBuf::from(format!(
                "{}/R/x86_64-pc-linux-gnu-library/4.4",
                home
            )));
            paths.push(PathBuf::from(format!(
                "{}/R/x86_64-pc-linux-gnu-library/4.3",
                home
            )));
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
        let has_base = paths.iter().any(|p| p.join("base").exists());
        assert!(
            has_base,
            "Should have at least one library path containing base package"
        );
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
        assert!(
            exports.contains(&"print".to_string()),
            "Should contain 'print'"
        );
        assert!(exports.contains(&"cat".to_string()), "Should contain 'cat'");
        assert!(exports.contains(&"c".to_string()), "Should contain 'c'");
        assert!(
            exports.contains(&"list".to_string()),
            "Should contain 'list'"
        );
        assert!(
            exports.contains(&"function".to_string()) || exports.contains(&"length".to_string()),
            "Should contain common base functions"
        );
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
        assert!(
            exports.contains(&"t.test".to_string()),
            "Should contain 't.test'"
        );
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
        assert!(
            exports.contains(&"head".to_string()),
            "Should contain 'head'"
        );
        assert!(
            exports.contains(&"tail".to_string()),
            "Should contain 'tail'"
        );
        assert!(exports.contains(&"str".to_string()), "Should contain 'str'");
    }

    #[tokio::test]
    async fn test_get_package_exports_nonexistent_package() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess
            .get_package_exports("nonexistent_package_xyz_123")
            .await;
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

        let result = subprocess
            .get_package_depends("nonexistent_package_xyz_123")
            .await;
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
            "base",
            "methods",
            "utils",
            "grDevices",
            "graphics",
            "stats",
            "datasets",
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
            subprocess
                .get_package_exports("nonexistent_pkg_12345")
                .await,
            subprocess.get_package_depends("").await,
            subprocess.get_package_depends("invalid;name").await,
            subprocess
                .get_package_depends("nonexistent_pkg_12345")
                .await,
        ];

        for result in results {
            assert!(
                result.is_err(),
                "Invalid operations should return Err, not panic"
            );
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
