// r_subprocess.rs - R subprocess interface for package queries
//
// This module provides an async interface for querying R about packages,
// library paths, and exports. It's used by the package function awareness
// feature to resolve package symbols.
//
// # Safety invariants for callers
//
// Any code that spawns an R subprocess to evaluate code or query R state (here
// or in sibling modules like `package_library`) MUST observe:
//
// 1. **Validate user-controlled inputs** (e.g. package names) before they
//    reach the spawn. Package-name validation is the canonical example.
// 2. **Wrap every R subprocess query in `tokio::time::timeout()`.** A hung
//    R process must not block the LSP indefinitely. Routing through
//    `execute_r_code{,_with_timeout}` satisfies both this and the global
//    concurrency bound below; do not spawn `R` directly past those helpers.
// 3. **Go through `execute_r_code`/`execute_r_code_with_timeout`** rather than
//    spawning `R` by hand. They hold a global semaphore (see
//    `r_subprocess_semaphore`) that caps how many R processes run at once.
//    Each spawn is CPU-heavy (base-package loading alone is 6–11s and pins a
//    core); without the cap a burst of callers oversubscribes every core and
//    starves the latency-sensitive 5s `formals()` queries past their timeout.
// 4. `RSubprocess::new` is the one direct-spawn carve-out: it may probe
//    candidate executables with `R --version` before an `RSubprocess` exists.
//    Keep that path side-effect-free: no `-e`, no package loading, and no
//    user-controlled R code.
// 5. **Never interpolate user-controlled strings into R code.** Pass values
//    as `Command` args instead. `help()` uses NSE for `package`, so any
//    variable argument MUST be wrapped in parens to force evaluation:
//    `help(topic, package = (pkg))`. Without the parens R reads the symbol
//    literally and a user-supplied package name silently fails to resolve.

use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::parameter_resolver::ParameterInfo;

/// Global bound on the number of R subprocesses running concurrently.
///
/// Every R spawn is CPU-heavy (loading the base packages alone takes
/// 6–11s of wall time and pins a core). Nothing previously limited how many
/// ran at once, so under high task concurrency (e.g. a 16-way test run, or a
/// burst of LSP requests each warming packages) dozens of R processes would
/// saturate every core simultaneously. That CPU starvation made short,
/// latency-sensitive queries — the 5s-budget `formals()` completion lookups —
/// blow past their own timeout, and starved unrelated CPU-bound work on the
/// same machine. Bounding concurrency to roughly the core count keeps each R
/// process making progress instead of thrashing, which both stabilises the
/// 5s timeout and protects co-scheduled CPU work.
///
/// The permit is acquired *outside* the per-call `tokio::time::timeout`, so
/// queue-wait never counts against a query's timeout — the timeout continues
/// to bound only the actual spawn-and-wait, preserving the "a hung R process
/// must not block the LSP indefinitely" invariant.
fn r_subprocess_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| {
        let permits = semaphore_permits(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        );
        Semaphore::new(permits)
    })
}

/// Permit count for the global R-subprocess semaphore: the machine's
/// available parallelism (fallback 4 when unknown), clamped to [2, 8].
fn semaphore_permits(parallelism: usize) -> usize {
    parallelism.clamp(2, 8)
}

/// R subprocess interface for package queries
pub struct RSubprocess {
    /// Path to R executable
    r_path: PathBuf,
    /// Working directory for R subprocess
    working_dir: Option<PathBuf>,
}

/// Stable identity of the executable backing one configured R runtime.
///
/// The requested path alone is insufficient: an installer can replace the
/// executable in place, and a symlink can be retargeted without changing the
/// configured string. Detached R consumers capture this identity before
/// spawning and compare it again at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RRuntimeIdentity {
    requested_path: PathBuf,
    canonical_path: Option<PathBuf>,
    disk: RRuntimeDiskIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RRuntimeDiskIdentity {
    Present {
        len: u64,
        modified: Option<std::time::SystemTime>,
        created: Option<std::time::SystemTime>,
        readonly: bool,
        #[cfg(unix)]
        device: u64,
        #[cfg(unix)]
        inode: u64,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        change_seconds: i64,
        #[cfg(unix)]
        change_nanoseconds: i64,
    },
    Missing,
    Invalid,
}

impl RSubprocess {
    #[cfg(test)]
    pub(crate) fn new_for_test(r_path: PathBuf) -> Self {
        Self {
            r_path,
            working_dir: None,
        }
    }

    /// Creates a configured RSubprocess when an R executable path can be validated or discovered.
    ///
    /// If `r_path` is `Some(path)`, the provided path is validated as an R executable and used on success.
    /// If `r_path` is `None`, the function attempts to discover an R executable in the environment or common locations.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use raven::r_subprocess::RSubprocess;
    /// // If an explicit invalid path is given, `new` returns `None`.
    /// assert!(RSubprocess::new(Some(PathBuf::from("/no/such/path"))).is_none());
    /// // When no path is provided, `new` attempts discovery and may return `Some` or `None` depending on the environment.
    /// let _ = RSubprocess::new(None);
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

    pub(crate) fn runtime_identity(&self) -> RRuntimeIdentity {
        let canonical_path = match std::fs::canonicalize(&self.r_path) {
            Ok(path) => Some(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return RRuntimeIdentity {
                    requested_path: self.r_path.clone(),
                    canonical_path: None,
                    disk: RRuntimeDiskIdentity::Missing,
                };
            }
            Err(_) => {
                return RRuntimeIdentity {
                    requested_path: self.r_path.clone(),
                    canonical_path: None,
                    disk: RRuntimeDiskIdentity::Invalid,
                };
            }
        };
        let metadata = match std::fs::metadata(
            canonical_path
                .as_ref()
                .expect("successful canonicalization has a path"),
        ) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return RRuntimeIdentity {
                    requested_path: self.r_path.clone(),
                    canonical_path,
                    disk: RRuntimeDiskIdentity::Invalid,
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return RRuntimeIdentity {
                    requested_path: self.r_path.clone(),
                    canonical_path,
                    disk: RRuntimeDiskIdentity::Missing,
                };
            }
            Err(_) => {
                return RRuntimeIdentity {
                    requested_path: self.r_path.clone(),
                    canonical_path,
                    disk: RRuntimeDiskIdentity::Invalid,
                };
            }
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        RRuntimeIdentity {
            requested_path: self.r_path.clone(),
            canonical_path,
            disk: RRuntimeDiskIdentity::Present {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
                readonly: metadata.permissions().readonly(),
                #[cfg(unix)]
                device: metadata.dev(),
                #[cfg(unix)]
                inode: metadata.ino(),
                #[cfg(unix)]
                mode: metadata.mode(),
                #[cfg(unix)]
                change_seconds: metadata.ctime(),
                #[cfg(unix)]
                change_nanoseconds: metadata.ctime_nsec(),
            },
        }
    }

    /// Locate an R executable on the system by searching common locations.
    ///
    /// Attempts to find an R binary first via the system PATH and then by checking
    /// a set of typical installation locations for the current platform.
    ///
    /// # Returns
    ///
    /// `Some(PathBuf)` containing the path to an R executable if found, `None` if no candidate was discovered.
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
    fn find_r_in_common_locations() -> Option<PathBuf> {
        let common_paths = Self::get_common_r_paths();
        common_paths.into_iter().find(Self::is_valid_r_executable)
    }

    /// Lists common filesystem locations where an R executable is typically installed for the current target OS.
    ///
    /// The returned list contains platform-specific candidate paths (macOS, Linux, Windows) in a preferred order.
    /// Entries are suggestions and may point to non-existent files; callers should validate existence before use.
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
        // Bound the number of R processes running at once (see
        // `r_subprocess_semaphore`). Acquired before — and held across — the
        // timeout below so queue-wait is excluded from the per-call timeout
        // budget. The semaphore is never closed, so `acquire` cannot error.
        let _permit = r_subprocess_semaphore()
            .acquire()
            .await
            .expect("R subprocess semaphore is never closed");

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
    /// # use raven::r_subprocess::RSubprocess;
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
    /// # use raven::r_subprocess::RSubprocess;
    /// # async fn doc_example() {
    /// let rp = RSubprocess::new(None).expect("R executable not found");
    /// let exports = rp.get_package_exports("stats").await.expect("failed to get exports");
    /// assert!(exports.iter().any(|s| s == "lm"));
    /// # }
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

        // Validate every name and build the quoted `c(...)` body (single-sources
        // the R code-injection guard this module's safety contract requires).
        let packages_vector = validate_and_join_package_names(packages)?;

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

    /// Enumerate dataset objects for multiple packages in one R call via
    /// `data(package = "pkg")$results[, "Item"]` (issue #429).
    ///
    /// Items are `name` or `name (stem)`; both halves are preserved in
    /// [`DataObject`] so callers can map file stems to bound object names.
    /// Packages that error (not installed, no data) yield empty lists.
    pub async fn get_multiple_package_datasets(
        &self,
        packages: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<DataObject>>> {
        if packages.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let packages_vector = validate_and_join_package_names(packages)?;
        let r_code = format!(
            r#"
pkgs <- c({})
cat("__RAVEN_MULTI_DATASETS__\n")
for (pkg in pkgs) {{
    cat(paste0("__PKG:", pkg, "__\n"))
    tryCatch({{
        items <- suppressWarnings(data(package = pkg)$results)
        if (length(items)) writeLines(items[, "Item"])
    }}, error = function(e) {{}})
}}
cat("__RAVEN_END__\n")
"#,
            packages_vector
        );
        let output = self.execute_r_code(&r_code).await?;
        parse_multi_datasets_output(&output)
    }

    /// Query function parameters using `formals()`.
    ///
    /// Resolves the function object and extracts its formal parameters.
    /// For primitive/special functions, falls back to `formals(args(fn))`.
    ///
    /// # Parameters
    ///
    /// - `function_name` — Name of the function to query (e.g., `"filter"`, `"na.rm"`)
    /// - `package` — Optional package name (e.g., `Some("dplyr")` for `dplyr::filter`)
    /// - `exported_only` — If `true`, uses `getExportedValue()` (for `::`);
    ///   if `false`, uses `asNamespace()` (for `:::` internal access)
    ///
    /// # Returns
    ///
    /// A `Vec<ParameterInfo>` with parameter names, default values, and dots detection.
    /// Returns an empty vec if the function has no formals (e.g., some primitives).
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    /// - Function or package name fails validation (code injection prevention)
    /// - R subprocess times out (5s timeout for completion-path queries)
    /// - R subprocess returns an error marker
    ///
    /// # Requirements: 10.1, 10.2, 10.3, 10.4, 10.5
    pub async fn get_function_formals(
        &self,
        function_name: &str,
        package: Option<&str>,
        exported_only: bool,
    ) -> Result<Vec<ParameterInfo>> {
        // Validate function name to prevent R code injection
        if !is_valid_r_identifier(function_name) {
            return Err(anyhow!(
                "Invalid function name '{}': must contain only letters, numbers, dots, and underscores",
                function_name
            ));
        }

        // Validate package name if provided
        if let Some(pkg) = package
            && !is_valid_package_name(pkg)
        {
            return Err(anyhow!(
                "Invalid package name '{}': must contain only letters, numbers, dots, and underscores",
                pkg
            ));
        }

        // Build R code to resolve the function and extract formals.
        // The function resolution strategy:
        // - No package: get(func_name, mode = "function") from the search path
        // - Exported (::): getExportedValue(pkg, func_name)
        // - Internal (:::): get(func_name, envir = asNamespace(pkg))
        //
        // For primitives, formals() returns NULL, so we fall back to formals(args(fn)).
        let r_code = match package {
            Some(pkg) => {
                if exported_only {
                    format!(
                        r#"tryCatch({{ fn <- getExportedValue("{pkg}", "{func}"); f <- if (is.primitive(fn)) formals(args(fn)) else formals(fn); if (is.null(f)) cat("") else for (name in names(f)) {{ default <- if (is.symbol(f[[name]]) && nchar(as.character(f[[name]])) == 0) "" else deparse(f[[name]], width.cutoff = 500)[1]; cat(name, "\t", default, "\n", sep = "") }} }}, error = function(e) cat("__RLSP_ERROR__:", conditionMessage(e), sep = ""))"#,
                        pkg = pkg,
                        func = function_name,
                    )
                } else {
                    format!(
                        r#"tryCatch({{ fn <- get("{func}", envir = asNamespace("{pkg}")); f <- if (is.primitive(fn)) formals(args(fn)) else formals(fn); if (is.null(f)) cat("") else for (name in names(f)) {{ default <- if (is.symbol(f[[name]]) && nchar(as.character(f[[name]])) == 0) "" else deparse(f[[name]], width.cutoff = 500)[1]; cat(name, "\t", default, "\n", sep = "") }} }}, error = function(e) cat("__RLSP_ERROR__:", conditionMessage(e), sep = ""))"#,
                        pkg = pkg,
                        func = function_name,
                    )
                }
            }
            None => {
                format!(
                    r#"tryCatch({{ fn <- get("{func}", mode = "function"); f <- if (is.primitive(fn)) formals(args(fn)) else formals(fn); if (is.null(f)) cat("") else for (name in names(f)) {{ default <- if (is.symbol(f[[name]]) && nchar(as.character(f[[name]])) == 0) "" else deparse(f[[name]], width.cutoff = 500)[1]; cat(name, "\t", default, "\n", sep = "") }} }}, error = function(e) cat("__RLSP_ERROR__:", conditionMessage(e), sep = ""))"#,
                    func = function_name,
                )
            }
        };

        // Use 5s timeout for completion-path queries (shorter than the default 30s)
        let timeout = std::time::Duration::from_secs(5);
        let output = self.execute_r_code_with_timeout(&r_code, timeout).await?;

        // Check if R returned an error
        if output.starts_with("__RLSP_ERROR__:") {
            let error_msg = output.trim_start_matches("__RLSP_ERROR__:").trim();
            return Err(anyhow!(
                "Failed to get formals for function '{}': {}",
                function_name,
                error_msg
            ));
        }

        // Parse the tab-separated output
        let params = parse_formals_output(&output);

        log::trace!(
            "Got {} formals for function '{}' (package: {:?}): {:?}",
            params.len(),
            function_name,
            package,
            params.iter().map(|p| &p.name).collect::<Vec<_>>()
        );

        Ok(params)
    }
}

/// Validate every package name against the R code-injection guard
/// ([`is_valid_package_name`]) and build the quoted, comma-joined body for an
/// R `c(...)` vector. Single source for the validate-then-interpolate idiom the
/// `r_subprocess` module doc requires of every caller that names packages.
fn validate_and_join_package_names(packages: &[String]) -> Result<String> {
    for pkg in packages {
        if !is_valid_package_name(pkg) {
            return Err(anyhow!(
                "Invalid package name '{}': must contain only letters, numbers, dots, and underscores",
                pkg
            ));
        }
    }
    Ok(packages
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", "))
}

/// Parse marker-framed multi-package R output into a per-package map.
///
/// The framing is shared by every batched query: a `header` line, then one
/// `__PKG:name__` line per package followed by that package's item lines, and a
/// final `__RAVEN_END__` terminator (tolerated absent). Each item line is fed
/// through `transform`; lines mapping to `None` are skipped. Returns an error
/// only if `header` is missing from `output`.
fn parse_multi_package_output<T>(
    output: &str,
    header: &str,
    transform: impl Fn(&str) -> Option<T>,
) -> Result<std::collections::HashMap<String, Vec<T>>> {
    let mut result = std::collections::HashMap::new();

    let start = output
        .find(header)
        .ok_or_else(|| anyhow!("Missing {header} marker in R output"))?;
    let section = &output[start + header.len()..];

    let mut current_package: Option<String> = None;
    let mut current_items: Vec<T> = Vec::new();

    for line in section.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with("__PKG:") && line.ends_with("__") {
            // Save previous package's items, start a new package.
            if let Some(pkg) = current_package.take() {
                result.insert(pkg, std::mem::take(&mut current_items));
            }
            current_package = Some(line[6..line.len() - 2].to_string()); // strip __PKG: and __
        } else if line == "__RAVEN_END__" {
            if let Some(pkg) = current_package.take() {
                result.insert(pkg, std::mem::take(&mut current_items));
            }
            break;
        } else if current_package.is_some()
            && let Some(item) = transform(line)
        {
            current_items.push(item);
        }
    }

    // Handle a missing __RAVEN_END__ terminator.
    if let Some(pkg) = current_package {
        result.insert(pkg, std::mem::take(&mut current_items));
    }

    Ok(result)
}

/// Parse the output of `get_multiple_package_exports()` into a HashMap.
fn parse_multi_exports_output(
    output: &str,
) -> Result<std::collections::HashMap<String, Vec<String>>> {
    let result = parse_multi_package_output(output, "__RAVEN_MULTI_EXPORTS__", |line| {
        Some(line.to_string())
    })?;
    log::trace!("Multi-export query: {} packages with exports", result.len());
    Ok(result)
}

/// One dataset object enumerated by `data(package = ...)`.
///
/// R's `Item` column is `name` for a dataset whose object name matches its
/// data-file stem, or `name (stem)` when a multi-object data file binds
/// differently-named objects (e.g. survey's `data/api.rda` → `apiclus1 (api)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataObject {
    /// The R object name bound by `data()` / lazy-loading (e.g. `apiclus1`).
    pub name: String,
    /// The data-file stem the object loads from (e.g. `api`); equals `name`
    /// when the Item carried no parenthesized topic.
    pub file_stem: String,
}

/// Parse one `Item` line from `data(package=)$results` into a [`DataObject`].
fn parse_data_item(line: &str) -> Option<DataObject> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some((name, rest)) = line.split_once(" (")
        && let Some(stem) = rest.strip_suffix(')')
    {
        let (name, stem) = (name.trim(), stem.trim());
        if !name.is_empty() && !stem.is_empty() {
            return Some(DataObject {
                name: name.to_string(),
                file_stem: stem.to_string(),
            });
        }
    }
    Some(DataObject {
        name: line.to_string(),
        file_stem: line.to_string(),
    })
}

/// Parse the marker-structured output of `get_multiple_package_datasets()`.
/// Same framing as [`parse_multi_exports_output`]: a `__RAVEN_MULTI_DATASETS__`
/// header, one `__PKG:name__` line per package, `__RAVEN_END__` terminator.
/// Item lines are parsed via [`parse_data_item`].
fn parse_multi_datasets_output(
    output: &str,
) -> Result<std::collections::HashMap<String, Vec<DataObject>>> {
    let result = parse_multi_package_output(output, "__RAVEN_MULTI_DATASETS__", parse_data_item)?;
    log::trace!("Multi-datasets query: {} packages", result.len());
    Ok(result)
}

/// Parse newline-separated R library paths into a vector of existing `PathBuf`s.
///
/// Trims each line, ignores empty lines, converts each remaining line into a `PathBuf`,
/// and retains only paths that exist on the filesystem.
///
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
pub(crate) fn is_valid_package_name(name: &str) -> bool {
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

/// Validate an R identifier (function name) for safe interpolation into R code.
///
/// R identifiers can contain ASCII letters, digits, dots, and underscores.
/// They must not be empty and must only contain characters matching `[a-zA-Z0-9._]`.
///
/// This is intentionally more permissive than `is_valid_package_name` (which
/// enforces R's package naming rules about first characters). Function names
/// in R can start with a dot followed by a digit (e.g., `.2way.interaction`),
/// so we only check the character set, not the starting character.
///
/// This validation prevents malicious input from being interpolated into R code.
fn is_valid_r_identifier(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    // All characters must match [a-zA-Z0-9._]
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() && c != '.' && c != '_' {
            return false;
        }
    }

    true
}

/// Parse tab-separated formals output from R subprocess.
///
/// Each line is `name\tdefault\n`. An empty string after the tab
/// (i.e., `name\t\n`) means the parameter has no default value
/// (`default_value = None`). A non-empty string after the tab is
/// the deparsed default value.
///
/// The `...` parameter is detected by name and sets `is_dots = true`.
fn parse_formals_output(output: &str) -> Vec<ParameterInfo> {
    output
        .lines()
        .filter_map(|line| {
            // Only trim trailing carriage returns, not tabs (tabs are delimiters)
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                return None;
            }

            // Split on first tab only
            let (name, default) = match line.split_once('\t') {
                Some((n, d)) => (n, d),
                None => {
                    // Malformed line (no tab separator) — skip
                    log::trace!("Skipping malformed formals line (no tab): {:?}", line);
                    return None;
                }
            };

            let name = name.trim().to_string();
            if name.is_empty() {
                return None;
            }

            let is_dots = name == "...";

            // Empty string after tab means no default value
            let default_value = if default.is_empty() {
                None
            } else {
                Some(default.to_string())
            };

            Some(ParameterInfo {
                name,
                default_value,
                is_dots,
            })
        })
        .collect()
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
/// use raven::r_subprocess::get_fallback_base_packages;
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

/// R's 14 base-priority packages (`installed.packages(priority="base")`): the
/// 7 default-attached ones ([`get_fallback_base_packages`]) plus the 7 that
/// ship with R but require an explicit `library()` to attach — `compiler`,
/// `grid`, `parallel`, `splines`, `stats4`, `tcltk`, `tools`. All 14 are
/// embedded so `library(grid)` etc. resolve offline (no R, no `names.db`); only
/// the default-attached 7 are always in scope.
pub fn get_base_priority_packages() -> Vec<String> {
    let mut pkgs = get_fallback_base_packages();
    pkgs.extend(
        [
            "compiler", "grid", "parallel", "splines", "stats4", "tcltk", "tools",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    pkgs
}

/// Platform-specific candidate R library directories used as a fallback when an R subprocess is unavailable.
///
/// This returns a curated list of common system, user, and package-manager library locations for macOS, Linux, and Windows,
/// filtered to only include paths that exist on the filesystem.
///
/// Note: this function performs synchronous filesystem checks (`Path::exists()`).
/// That is acceptable for the current fallback usage when the R subprocess is unavailable,
/// but avoid calling it on LSP request threads (do I/O off-thread and revalidate via cache updates).
///
/// # Examples
///
/// ```
/// use raven::r_subprocess::get_fallback_lib_paths;
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
    fn test_semaphore_permits_clamps_to_range() {
        // Below the floor clamps up to 2.
        assert_eq!(semaphore_permits(1), 2);
        assert_eq!(semaphore_permits(2), 2);
        // Within range passes through unchanged.
        assert_eq!(semaphore_permits(4), 4);
        assert_eq!(semaphore_permits(8), 8);
        // Above the ceiling clamps down to 8.
        assert_eq!(semaphore_permits(16), 8);
    }

    #[test]
    fn test_new_with_none_discovers_r() {
        // This test will pass if R is installed, skip otherwise
        let subprocess = RSubprocess::new(None);
        if let Some(subprocess) = subprocess {
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

    #[test]
    fn test_get_base_priority_packages() {
        let all = get_base_priority_packages();
        assert_eq!(all.len(), 14);
        let set: std::collections::HashSet<&str> = all.iter().map(String::as_str).collect();
        // Superset of the default-attached 7 plus the 7 library()-only packages.
        for p in get_fallback_base_packages() {
            assert!(set.contains(p.as_str()), "missing attached pkg {p}");
        }
        for p in [
            "compiler", "grid", "parallel", "splines", "stats4", "tcltk", "tools",
        ] {
            assert!(set.contains(p), "missing base-priority pkg {p}");
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

    // -----------------------------------------------------------------------
    // Tests for is_valid_r_identifier
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_valid_r_identifier_simple() {
        assert!(is_valid_r_identifier("filter"));
        assert!(is_valid_r_identifier("print"));
        assert!(is_valid_r_identifier("mean"));
    }

    #[test]
    fn test_is_valid_r_identifier_with_dots() {
        assert!(is_valid_r_identifier("na.rm"));
        assert!(is_valid_r_identifier("is.na"));
        assert!(is_valid_r_identifier("data.frame"));
        assert!(is_valid_r_identifier("as.character"));
    }

    #[test]
    fn test_is_valid_r_identifier_with_underscores() {
        assert!(is_valid_r_identifier("my_func"));
        assert!(is_valid_r_identifier("process_data"));
    }

    #[test]
    fn test_is_valid_r_identifier_with_numbers() {
        assert!(is_valid_r_identifier("ggplot2"));
        assert!(is_valid_r_identifier("utf8"));
    }

    #[test]
    fn test_is_valid_r_identifier_empty() {
        assert!(!is_valid_r_identifier(""));
    }

    #[test]
    fn test_is_valid_r_identifier_injection_attempts() {
        // These should all be rejected to prevent R code injection
        assert!(!is_valid_r_identifier("func; system('rm -rf /')"));
        assert!(!is_valid_r_identifier("func\"); system('ls')"));
        assert!(!is_valid_r_identifier("func$(whoami)"));
        assert!(!is_valid_r_identifier("func`ls`"));
        assert!(!is_valid_r_identifier("func\ncat('injected')"));
        assert!(!is_valid_r_identifier("func\rcat('injected')"));
        assert!(!is_valid_r_identifier("func'"));
        assert!(!is_valid_r_identifier("func\""));
        assert!(!is_valid_r_identifier("func("));
        assert!(!is_valid_r_identifier("func)"));
        assert!(!is_valid_r_identifier("func{"));
        assert!(!is_valid_r_identifier("func}"));
        assert!(!is_valid_r_identifier("func<-"));
        assert!(!is_valid_r_identifier("func="));
        assert!(!is_valid_r_identifier("func+"));
        assert!(!is_valid_r_identifier("func-"));
        assert!(!is_valid_r_identifier("func*"));
        assert!(!is_valid_r_identifier("func/"));
        assert!(!is_valid_r_identifier("func\\"));
        assert!(!is_valid_r_identifier("func|"));
        assert!(!is_valid_r_identifier("func&"));
        assert!(!is_valid_r_identifier("func!"));
        assert!(!is_valid_r_identifier("func@"));
        assert!(!is_valid_r_identifier("func#"));
        assert!(!is_valid_r_identifier("func$"));
        assert!(!is_valid_r_identifier("func%"));
        assert!(!is_valid_r_identifier("func^"));
        assert!(!is_valid_r_identifier("func~"));
        assert!(!is_valid_r_identifier("func "));
        assert!(!is_valid_r_identifier(" func"));
        assert!(!is_valid_r_identifier("func name"));
    }

    #[test]
    fn test_is_valid_r_identifier_unicode() {
        // Unicode characters should be rejected (only ASCII allowed)
        assert!(!is_valid_r_identifier("функция")); // Russian
        assert!(!is_valid_r_identifier("函数")); // Chinese
        assert!(!is_valid_r_identifier("func日本語"));
    }

    #[test]
    fn test_is_valid_r_identifier_dots_only() {
        // "..." is a valid R identifier (the dots parameter)
        assert!(is_valid_r_identifier("..."));
        // Single dot is also valid
        assert!(is_valid_r_identifier("."));
        // Dot followed by digit is valid for function names (unlike package names)
        assert!(is_valid_r_identifier(".1"));
        assert!(is_valid_r_identifier(".123"));
    }

    // -----------------------------------------------------------------------
    // Tests for parse_formals_output
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_formals_output_simple() {
        let output = "x\t\ny\t\n";
        let params = parse_formals_output(output);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "x");
        assert!(params[0].default_value.is_none());
        assert!(!params[0].is_dots);
        assert_eq!(params[1].name, "y");
        assert!(params[1].default_value.is_none());
        assert!(!params[1].is_dots);
    }

    #[test]
    fn test_parse_formals_output_with_defaults() {
        let output = "x\t1\ny\tTRUE\nz\t\"hello\"\n";
        let params = parse_formals_output(output);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "x");
        assert_eq!(params[0].default_value.as_deref(), Some("1"));
        assert_eq!(params[1].name, "y");
        assert_eq!(params[1].default_value.as_deref(), Some("TRUE"));
        assert_eq!(params[2].name, "z");
        assert_eq!(params[2].default_value.as_deref(), Some("\"hello\""));
    }

    #[test]
    fn test_parse_formals_output_with_dots() {
        let output = "x\t\n...\t\ny\t1\n";
        let params = parse_formals_output(output);
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "x");
        assert!(!params[0].is_dots);
        assert_eq!(params[1].name, "...");
        assert!(params[1].is_dots);
        assert!(params[1].default_value.is_none());
        assert_eq!(params[2].name, "y");
        assert_eq!(params[2].default_value.as_deref(), Some("1"));
    }

    #[test]
    fn test_parse_formals_output_empty() {
        let output = "";
        let params = parse_formals_output(output);
        assert!(params.is_empty());
    }

    #[test]
    fn test_parse_formals_output_whitespace_only() {
        let output = "  \n  \n";
        let params = parse_formals_output(output);
        assert!(params.is_empty());
    }

    #[test]
    fn test_parse_formals_output_malformed_no_tab() {
        // Lines without tab separators should be skipped
        let output = "x\t\nmalformed_line\ny\t1\n";
        let params = parse_formals_output(output);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "x");
        assert_eq!(params[1].name, "y");
    }

    #[test]
    fn test_parse_formals_output_complex_defaults() {
        let output = "formula\t\ndata\tNULL\nsubset\t\nweights\t\nna.action\t\nmethod\t\"qr\"\n";
        let params = parse_formals_output(output);
        assert_eq!(params.len(), 6);
        assert_eq!(params[0].name, "formula");
        assert!(params[0].default_value.is_none());
        assert_eq!(params[1].name, "data");
        assert_eq!(params[1].default_value.as_deref(), Some("NULL"));
        assert_eq!(params[5].name, "method");
        assert_eq!(params[5].default_value.as_deref(), Some("\"qr\""));
    }

    // -----------------------------------------------------------------------
    // Tests for get_function_formals (integration with R subprocess)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_function_formals_base_print() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let params = subprocess
            .get_function_formals("print", Some("base"), true)
            .await
            .expect("Should get formals for base::print");

        // print.default has x and ... parameters
        assert!(!params.is_empty(), "print should have parameters");
        assert_eq!(params[0].name, "x", "First param should be 'x'");
        // print has a ... parameter
        assert!(
            params.iter().any(|p| p.is_dots),
            "print should have a ... parameter"
        );
    }

    #[tokio::test]
    async fn test_get_function_formals_base_mean() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // mean is a primitive, so formals(args(fn)) should be used
        let params = subprocess
            .get_function_formals("mean", None, true)
            .await
            .expect("Should get formals for mean");

        assert!(!params.is_empty(), "mean should have parameters");
        assert_eq!(params[0].name, "x", "First param should be 'x'");
    }

    #[tokio::test]
    async fn test_get_function_formals_stats_lm() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let params = subprocess
            .get_function_formals("lm", Some("stats"), true)
            .await
            .expect("Should get formals for stats::lm");

        // lm has formula, data, subset, weights, na.action, method, etc.
        assert!(params.len() >= 5, "lm should have at least 5 parameters");
        assert_eq!(params[0].name, "formula", "First param should be 'formula'");

        // Check that data has a default of NULL (or similar)
        let data_param = params.iter().find(|p| p.name == "data");
        assert!(data_param.is_some(), "lm should have a 'data' parameter");
    }

    #[tokio::test]
    async fn test_get_function_formals_unqualified() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // Query without package qualification
        let params = subprocess
            .get_function_formals("paste", None, true)
            .await
            .expect("Should get formals for paste");

        assert!(!params.is_empty(), "paste should have parameters");
        assert!(
            params.iter().any(|p| p.is_dots),
            "paste should have a ... parameter"
        );
        // paste has sep and collapse parameters
        assert!(
            params.iter().any(|p| p.name == "sep"),
            "paste should have a 'sep' parameter"
        );
    }

    #[tokio::test]
    async fn test_get_function_formals_nonexistent_function() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess
            .get_function_formals("nonexistent_func_xyz_12345", None, true)
            .await;

        assert!(
            result.is_err(),
            "Nonexistent function should return an error"
        );
    }

    #[tokio::test]
    async fn test_get_function_formals_invalid_function_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess
            .get_function_formals("func; system('ls')", None, true)
            .await;

        assert!(
            result.is_err(),
            "Invalid function name should return an error"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid function name"),
            "Error should mention invalid function name: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_get_function_formals_invalid_package_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess
            .get_function_formals("print", Some("pkg; system('ls')"), true)
            .await;

        assert!(
            result.is_err(),
            "Invalid package name should return an error"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid package name"),
            "Error should mention invalid package name: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_get_function_formals_empty_function_name() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let result = subprocess.get_function_formals("", None, true).await;

        assert!(
            result.is_err(),
            "Empty function name should return an error"
        );
    }

    #[tokio::test]
    async fn test_get_function_formals_internal_access() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // Test with exported_only = false (:::  access)
        let params = subprocess
            .get_function_formals("print", Some("base"), false)
            .await
            .expect("Should get formals for base:::print");

        assert!(!params.is_empty(), "print should have parameters");
    }

    #[tokio::test]
    async fn test_get_function_formals_primitive_sum() {
        // Skip if R is not available
        let subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        // sum is a primitive function — should fall back to formals(args(fn))
        let params = subprocess
            .get_function_formals("sum", None, true)
            .await
            .expect("Should get formals for sum (primitive)");

        assert!(!params.is_empty(), "sum should have parameters via args()");
        assert!(
            params.iter().any(|p| p.is_dots),
            "sum should have a ... parameter"
        );
    }

    #[test]
    fn test_parse_data_item_plain() {
        assert_eq!(
            parse_data_item("lung"),
            Some(DataObject {
                name: "lung".to_string(),
                file_stem: "lung".to_string()
            })
        );
    }

    #[test]
    fn test_parse_data_item_aliased() {
        assert_eq!(
            parse_data_item("apiclus1 (api)"),
            Some(DataObject {
                name: "apiclus1".to_string(),
                file_stem: "api".to_string()
            })
        );
    }

    #[test]
    fn test_parse_data_item_empty_and_whitespace() {
        assert_eq!(parse_data_item(""), None);
        assert_eq!(parse_data_item("   "), None);
    }

    #[test]
    fn test_parse_multi_datasets_output_two_packages() {
        let output = "\
__RAVEN_MULTI_DATASETS__
__PKG:survival__
lung
ovarian
__PKG:survey__
apiclus1 (api)
apistrat (api)
__RAVEN_END__
";
        let result = parse_multi_datasets_output(output).unwrap();
        assert_eq!(result["survival"].len(), 2);
        assert_eq!(result["survival"][0].name, "lung");
        assert_eq!(
            result["survey"][0],
            DataObject {
                name: "apiclus1".into(),
                file_stem: "api".into()
            }
        );
        assert_eq!(result["survey"][1].file_stem, "api");
    }

    #[test]
    fn test_parse_multi_datasets_output_empty_package() {
        let output = "__RAVEN_MULTI_DATASETS__\n__PKG:cli__\n__RAVEN_END__\n";
        let result = parse_multi_datasets_output(output).unwrap();
        assert!(result["cli"].is_empty());
    }

    #[test]
    fn test_parse_multi_datasets_output_missing_marker() {
        assert!(parse_multi_datasets_output("no marker here").is_err());
    }

    #[test]
    fn test_validate_and_join_package_names_quotes_and_rejects() {
        // R-free coverage of the helper that single-sources the injection guard
        // AND builds the quoted `c(...)` body interpolated into R code (issue
        // #429). The R-gated batch tests skip without R, so this pins the
        // output contract and the reject path in CI.
        assert_eq!(
            validate_and_join_package_names(&["dplyr".to_string(), "ggplot2".to_string()]).unwrap(),
            "\"dplyr\", \"ggplot2\"",
            "valid names must be quoted and comma-joined for the R vector body"
        );
        assert_eq!(
            validate_and_join_package_names(&["survey".to_string()]).unwrap(),
            "\"survey\"",
            "single valid name must be quoted"
        );
        // An injection attempt is rejected before any interpolation.
        assert!(
            validate_and_join_package_names(&["bad; system('x')".to_string()]).is_err(),
            "names with shell/R metacharacters must be rejected"
        );
        // One invalid name among valid ones rejects the whole batch (fail-closed).
        assert!(
            validate_and_join_package_names(&["dplyr".to_string(), "a\"b".to_string()]).is_err(),
            "a quote-bearing name must reject the batch so it cannot break out of the R string"
        );
    }

    #[tokio::test]
    async fn test_get_multiple_package_datasets_base_datasets() {
        // `datasets` ships with every R install; `state.abb (state)` exercises aliases.
        let Some(sub) = RSubprocess::new(None) else {
            eprintln!("R not available, skipping");
            return;
        };
        let result = sub
            .get_multiple_package_datasets(&["datasets".to_string()])
            .await
            .expect("query should succeed");
        let items = &result["datasets"];
        assert!(
            items
                .iter()
                .any(|d| d.name == "mtcars" && d.file_stem == "mtcars")
        );
        assert!(
            items
                .iter()
                .any(|d| d.name == "state.abb" && d.file_stem == "state")
        );
    }

    #[tokio::test]
    async fn test_get_multiple_package_datasets_rejects_invalid_name() {
        // Validation fires before any subprocess is spawned, but constructing
        // `RSubprocess` still requires R on PATH, so the test is skipped
        // (like its neighbors) when R is absent.
        let Some(sub) = RSubprocess::new(None) else {
            eprintln!("R not available, skipping");
            return;
        };
        assert!(
            sub.get_multiple_package_datasets(&["bad; system('x')".to_string()])
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    // Feature: function-parameter-completions, Property 11: R Subprocess Input Validation
    //
    // For any function name containing characters outside [a-zA-Z0-9._] or starting
    // with invalid characters, the R subprocess query methods SHALL reject the input
    // without executing R code.
    //
    // **Validates: Requirements 10.2**

    /// Strategy to generate strings that contain at least one character outside [a-zA-Z0-9._].
    /// These should always be rejected by is_valid_r_identifier.
    fn invalid_r_identifier() -> impl Strategy<Value = String> {
        // Generate a string with at least one invalid character
        prop_oneof![
            // Strings containing injection-dangerous characters
            "[a-zA-Z0-9._]{0,5}[;()\"'`\\\\{}\\[\\]!@#$%^&*<>|/~, \\-+?:][a-zA-Z0-9._]{0,5}",
            // Strings with spaces
            "[a-zA-Z]{1,3} [a-zA-Z]{1,3}",
            // Strings with semicolons (R code injection)
            "[a-zA-Z]{1,3};[a-zA-Z]{1,3}",
            // Strings with parentheses (function call injection)
            "[a-zA-Z]{1,3}\\([a-zA-Z]{0,3}\\)",
            // Strings with quotes (string injection)
            "[a-zA-Z]{1,3}\"[a-zA-Z]{0,3}",
            // Strings with newlines/control characters
            "[a-zA-Z]{1,3}\n[a-zA-Z]{0,3}",
            // Unicode characters (non-ASCII)
            "[a-zA-Z]{1,3}[àéîöü][a-zA-Z]{0,3}",
        ]
        .prop_filter("must not be empty", |s| !s.is_empty())
        .prop_filter("must contain at least one invalid char", |s| {
            s.chars()
                .any(|c| !c.is_ascii_alphanumeric() && c != '.' && c != '_')
        })
    }

    /// Strategy to generate valid R identifiers (only [a-zA-Z0-9._], non-empty).
    fn valid_r_identifier() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9._]{1,20}".prop_filter("must not be empty", |s| !s.is_empty())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 11a: Function names with characters outside [a-zA-Z0-9._] are rejected.
        ///
        /// **Validates: Requirements 10.2**
        #[test]
        fn invalid_identifiers_are_rejected(name in invalid_r_identifier()) {
            prop_assert!(
                !is_valid_r_identifier(&name),
                "is_valid_r_identifier should reject '{}' which contains characters outside [a-zA-Z0-9._]",
                name
            );
        }

        /// Property 11b: Function names with only [a-zA-Z0-9._] characters are accepted.
        ///
        /// **Validates: Requirements 10.2**
        #[test]
        fn valid_identifiers_are_accepted(name in valid_r_identifier()) {
            prop_assert!(
                is_valid_r_identifier(&name),
                "is_valid_r_identifier should accept '{}' which contains only [a-zA-Z0-9._]",
                name
            );
        }

        /// Property 11c: Empty strings are always rejected.
        ///
        /// **Validates: Requirements 10.2**
        #[test]
        fn empty_string_is_rejected(_dummy in 0..1u32) {
            prop_assert!(
                !is_valid_r_identifier(""),
                "is_valid_r_identifier should reject empty strings"
            );
        }
    }
}
