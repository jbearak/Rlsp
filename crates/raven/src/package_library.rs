// package_library.rs - Package library manager for package function awareness
//
// This module provides the PackageLibrary struct which manages installed R packages,
// their exports, and caching. It integrates with the R subprocess interface for
// querying package information and falls back to NAMESPACE file parsing when needed.
//
// Requirement 13.1: THE Package_Cache SHALL store parsed exports per package
// Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers

// Allow dead code during incremental development - this module will be
// integrated into WorldState in task 7.1
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::r_subprocess::RSubprocess;

/// Meta-packages that attach multiple other packages when loaded
///
/// Requirement 4.3: WHEN the package is `tidyverse`, THE Package_Resolver SHALL also load
/// exports from: dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr
pub const TIDYVERSE_PACKAGES: &[&str] = &[
    "dplyr",
    "readr",
    "forcats",
    "stringr",
    "ggplot2",
    "tibble",
    "lubridate",
    "tidyr",
    "purrr",
];

/// Requirement 4.4: WHEN the package is `tidymodels`, THE Package_Resolver SHALL also load
/// exports from: broom, dials, dplyr, ggplot2, infer, modeldata, parsnip, purrr, recipes,
/// rsample, tibble, tidyr, tune, workflows, workflowsets, yardstick
pub const TIDYMODELS_PACKAGES: &[&str] = &[
    "broom",
    "dials",
    "dplyr",
    "ggplot2",
    "infer",
    "modeldata",
    "parsnip",
    "purrr",
    "recipes",
    "rsample",
    "tibble",
    "tidyr",
    "tune",
    "workflows",
    "workflowsets",
    "yardstick",
];

/// Cached package information
///
/// Stores all relevant information about an R package including its exports,
/// dependencies, and meta-package status.
#[derive(Debug, Clone)]
pub struct PackageInfo {
    /// Package name
    pub name: String,
    /// Exported symbols (functions, variables, and datasets)
    pub exports: HashSet<String>,
    /// Packages from Depends field
    pub depends: Vec<String>,
    /// Whether this is a meta-package with special handling
    pub is_meta_package: bool,
    /// Packages attached by meta-package (e.g., tidyverse attaches dplyr, ggplot2, etc.)
    pub attached_packages: Vec<String>,
    /// Lazy-loaded dataset names
    pub lazy_data: Vec<String>,
}

impl PackageInfo {
    /// Create a new PackageInfo with the given name and exports
    pub fn new(name: String, exports: HashSet<String>) -> Self {
        let is_meta_package = name == "tidyverse" || name == "tidymodels";
        let attached_packages = if name == "tidyverse" {
            TIDYVERSE_PACKAGES.iter().map(|s| s.to_string()).collect()
        } else if name == "tidymodels" {
            TIDYMODELS_PACKAGES.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };

        Self {
            name,
            exports,
            depends: Vec::new(),
            is_meta_package,
            attached_packages,
            lazy_data: Vec::new(),
        }
    }

    /// Create a new PackageInfo with all fields specified
    pub fn with_details(
        name: String,
        exports: HashSet<String>,
        depends: Vec<String>,
        lazy_data: Vec<String>,
    ) -> Self {
        let is_meta_package = name == "tidyverse" || name == "tidymodels";
        let attached_packages = if name == "tidyverse" {
            TIDYVERSE_PACKAGES.iter().map(|s| s.to_string()).collect()
        } else if name == "tidymodels" {
            TIDYMODELS_PACKAGES.iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };

        Self {
            name,
            exports,
            depends,
            is_meta_package,
            attached_packages,
            lazy_data,
        }
    }
}

/// Package library manager
///
/// Manages the collection of installed R packages and their cached exports.
/// Uses RwLock for thread-safe concurrent read access from multiple LSP handlers.
///
/// Requirement 13.1: THE Package_Cache SHALL store parsed exports per package
/// Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers
pub struct PackageLibrary {
    /// Library paths (from R or configuration)
    lib_paths: Vec<PathBuf>,
    /// Cached package information (lazy-loaded)
    /// Uses RwLock for thread-safe concurrent read access
    packages: RwLock<HashMap<String, Arc<PackageInfo>>>,
    /// Combined exports cache (package name -> all exports including Depends/attached)
    /// Populated by get_all_exports for efficient repeated lookups
    combined_exports: RwLock<HashMap<String, Arc<HashSet<String>>>>,
    /// Base packages (always available)
    base_packages: HashSet<String>,
    /// Base package exports (combined from all base packages)
    base_exports: HashSet<String>,
    /// R subprocess interface (None if R is unavailable)
    #[allow(dead_code)] // Will be used in task 3.3
    r_subprocess: Option<RSubprocess>,
}

impl PackageLibrary {
    /// Create a new PackageLibrary with default (empty) state
    ///
    /// This creates an uninitialized PackageLibrary. Call `initialize()` to
    /// populate it with data from R subprocess or fallback values.
    ///
    /// For a fully initialized PackageLibrary, use `PackageLibrary::new()` instead.
    pub fn new_empty() -> Self {
        Self {
            lib_paths: Vec::new(),
            packages: RwLock::new(HashMap::new()),
            combined_exports: RwLock::new(HashMap::new()),
            base_packages: HashSet::new(),
            base_exports: HashSet::new(),
            r_subprocess: None,
        }
    }

    /// Create a new PackageLibrary with the given R subprocess
    ///
    /// This is a synchronous constructor that sets up the PackageLibrary
    /// with the R subprocess interface. The actual initialization (querying
    /// R for lib_paths and base_packages) should be done via `initialize()`.
    ///
    /// # Arguments
    /// * `r_subprocess` - Optional R subprocess interface for querying package info
    pub fn with_subprocess(r_subprocess: Option<RSubprocess>) -> Self {
        Self {
            lib_paths: Vec::new(),
            packages: RwLock::new(HashMap::new()),
            combined_exports: RwLock::new(HashMap::new()),
            base_packages: HashSet::new(),
            base_exports: HashSet::new(),
            r_subprocess,
        }
    }

    /// Get the library paths
    pub fn lib_paths(&self) -> &[PathBuf] {
        &self.lib_paths
    }

    /// Get the base packages
    pub fn base_packages(&self) -> &HashSet<String> {
        &self.base_packages
    }

    /// Get the base exports
    pub fn base_exports(&self) -> &HashSet<String> {
        &self.base_exports
    }

    /// Check if a symbol is exported by base packages
    ///
    /// Requirement 6.3: THE Base_Packages SHALL be available at all positions
    /// in all files without requiring explicit library() calls
    pub fn is_base_export(&self, symbol: &str) -> bool {
        self.base_exports.contains(symbol)
    }

    /// Check if a package is a base package
    pub fn is_base_package(&self, package: &str) -> bool {
        self.base_packages.contains(package)
    }

    /// Get all exports from loaded packages for completions (synchronous, cached-only)
    ///
    /// This method returns a map of symbol name to package name for all exports
    /// from the given loaded packages. It uses cached package information,
    /// preferring combined_exports cache (includes Depends/attached) when available.
    ///
    /// This is a synchronous method suitable for use in completion handlers where
    /// we cannot use async. It uses `try_read()` to avoid blocking.
    ///
    /// Returns a HashMap where:
    /// - Key: export name (symbol)
    /// - Value: package name that exports it
    ///
    /// If multiple packages export the same symbol, the first package in the list wins.
    /// When multiple packages export the same symbol, all packages are included in the
    /// result vector for that symbol, in the order they were loaded.
    ///
    /// Requirements 9.1, 9.2, 9.3: Get package exports for completions with package attribution
    /// Requirement 9.3: When multiple packages export same symbol, show all with attribution
    pub fn get_exports_for_completions(
        &self,
        loaded_packages: &[String],
    ) -> std::collections::HashMap<String, Vec<String>> {
        let mut result: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        // Try combined_exports cache first (includes Depends/attached packages)
        let combined_cache = self.combined_exports.try_read().ok();
        let packages_cache = self.packages.try_read().ok();

        if combined_cache.is_none() && packages_cache.is_none() {
            log::trace!(
                "Could not acquire any package cache lock for completions, returning empty"
            );
            return result;
        }

        // Process packages in order (earlier packages appear first in the list)
        // Requirement 9.3: Show all packages that export the same symbol
        for pkg_name in loaded_packages {
            // Try combined_exports first
            if let Some(ref cache) = combined_cache {
                if let Some(exports) = cache.get(pkg_name) {
                    for export in exports.iter() {
                        result
                            .entry(export.clone())
                            .or_default()
                            .push(pkg_name.clone());
                    }
                    continue; // Found in combined cache, skip per-package lookup
                }
            }

            // Fall back to per-package exports
            if let Some(ref cache) = packages_cache {
                if let Some(info) = cache.get(pkg_name) {
                    for export in &info.exports {
                        result
                            .entry(export.clone())
                            .or_default()
                            .push(pkg_name.clone());
                    }
                }
            }
        }

        result
    }

    /// Check if a symbol is exported by any of the given packages (synchronous, cached-only)
    ///
    /// This method checks if the symbol is exported by any of the loaded packages,
    /// using cached package information. It first checks base exports, then
    /// checks combined_exports cache (includes Depends/attached), then falls back
    /// to per-package exports cache.
    ///
    /// This is a synchronous method suitable for use in diagnostic collection where
    /// we cannot use async. It uses `try_read()` to avoid blocking.
    ///
    /// Returns true if:
    /// - The symbol is a base export, OR
    /// - The symbol is exported by any of the loaded packages (from cache)
    ///
    /// Returns false if:
    /// - The symbol is not found in base exports or any cached loaded package
    /// - The cache lock cannot be acquired (returns false to be conservative)
    ///
    /// Requirements 8.1, 8.2: Check if symbol is exported by loaded packages at position
    pub fn is_symbol_from_loaded_packages(&self, symbol: &str, loaded_packages: &[String]) -> bool {
        // First check base exports (always available)
        if self.is_base_export(symbol) {
            return true;
        }

        // Try combined_exports cache first (includes Depends/attached packages)
        if let Ok(combined_cache) = self.combined_exports.try_read() {
            for pkg_name in loaded_packages {
                if let Some(exports) = combined_cache.get(pkg_name) {
                    if exports.contains(symbol) {
                        return true;
                    }
                }
            }
        }

        // Fall back to per-package exports cache
        let cache = match self.packages.try_read() {
            Ok(guard) => guard,
            Err(_) => {
                log::trace!(
                    "Could not acquire package cache lock for symbol '{}', returning false",
                    symbol
                );
                return false;
            }
        };

        // Check each loaded package
        for pkg_name in loaded_packages {
            if let Some(info) = cache.get(pkg_name) {
                if info.exports.contains(symbol) {
                    return true;
                }
            }
        }

        false
    }

    /// Get cached package info if available
    ///
    /// This is a synchronous method that only checks the cache.
    /// For loading packages that aren't cached, use `get_package()`.
    pub async fn get_cached_package(&self, name: &str) -> Option<Arc<PackageInfo>> {
        let cache = self.packages.read().await;
        cache.get(name).cloned()
    }

    /// Check if a package is cached
    pub async fn is_cached(&self, name: &str) -> bool {
        let cache = self.packages.read().await;
        cache.contains_key(name)
    }

    /// Get the number of cached packages
    pub async fn cached_count(&self) -> usize {
        let cache = self.packages.read().await;
        cache.len()
    }

    /// Insert a package into the cache
    ///
    /// This is primarily used for testing and initialization.
    pub async fn insert_package(&self, info: PackageInfo) {
        let mut cache = self.packages.write().await;
        cache.insert(info.name.clone(), Arc::new(info));
    }

    /// Invalidate cache for a package
    ///
    /// Removes the package from the cache, forcing it to be reloaded
    /// on the next access.
    pub async fn invalidate(&self, name: &str) {
        let mut cache = self.packages.write().await;
        cache.remove(name);
    }

    /// Clear all cached packages
    pub async fn clear_cache(&self) {
        let mut cache = self.packages.write().await;
        cache.clear();
    }

    /// Prefetch packages by loading their exports into cache
    ///
    /// This method asynchronously loads package exports for the given package names,
    /// populating both the per-package cache and combined_exports cache.
    /// Used for background warm-up after detecting library() calls.
    ///
    /// # Arguments
    /// * `packages` - List of package names to prefetch
    pub async fn prefetch_packages(&self, packages: &[String]) {
        for pkg_name in packages {
            log::trace!("Prefetching package exports for '{}'", pkg_name);
            // get_all_exports populates both caches
            let _ = self.get_all_exports(pkg_name).await;
        }
    }

    /// Add additional library paths (deduplicating)
    ///
    /// This method adds paths to the library search paths, avoiding duplicates.
    /// Used to apply user-configured additional library paths.
    pub fn add_library_paths(&mut self, paths: &[PathBuf]) {
        for path in paths {
            if !self.lib_paths.contains(path) {
                self.lib_paths.push(path.clone());
            }
        }
    }

    /// Get combined exports for a package from cache (synchronous)
    ///
    /// Returns the cached combined exports if available, None otherwise.
    /// This is useful for hover to check package exports without blocking.
    pub fn get_cached_combined_exports(&self, name: &str) -> Option<Arc<HashSet<String>>> {
        self.combined_exports.try_read().ok()?.get(name).cloned()
    }

    /// Find which package exports a symbol (synchronous, cached-only)
    ///
    /// Searches through loaded packages to find which one exports the given symbol.
    /// Returns the first package name that exports the symbol, or None.
    pub fn find_package_for_symbol(
        &self,
        symbol: &str,
        loaded_packages: &[String],
    ) -> Option<String> {
        // Check combined_exports cache first
        if let Ok(cache) = self.combined_exports.try_read() {
            for pkg_name in loaded_packages {
                if let Some(exports) = cache.get(pkg_name) {
                    if exports.contains(symbol) {
                        return Some(pkg_name.clone());
                    }
                }
            }
        }

        // Fall back to per-package cache
        if let Ok(cache) = self.packages.try_read() {
            for pkg_name in loaded_packages {
                if let Some(info) = cache.get(pkg_name) {
                    if info.exports.contains(symbol) {
                        return Some(pkg_name.clone());
                    }
                }
            }
        }

        None
    }

    /// Set the library paths
    ///
    /// This is used during initialization to set the library paths
    /// discovered from R or configuration.
    pub fn set_lib_paths(&mut self, paths: Vec<PathBuf>) {
        self.lib_paths = paths;
    }

    /// Set the base packages
    ///
    /// This is used during initialization to set the base packages
    /// discovered from R or fallback values.
    pub fn set_base_packages(&mut self, packages: HashSet<String>) {
        self.base_packages = packages;
    }

    /// Set the base exports
    ///
    /// This is used during initialization to set the combined exports
    /// from all base packages.
    pub fn set_base_exports(&mut self, exports: HashSet<String>) {
        self.base_exports = exports;
    }

    /// Check if a symbol is exported by a specific package
    ///
    /// Returns true if the package is cached and exports the symbol.
    /// Returns false if the package is not cached or doesn't export the symbol.
    pub async fn is_package_export(&self, symbol: &str, package: &str) -> bool {
        if let Some(info) = self.get_cached_package(package).await {
            info.exports.contains(symbol)
        } else {
            false
        }
    }

    /// Create a new PackageLibrary and initialize it with R subprocess query or fallback
    ///
    /// This is a convenience constructor that creates a PackageLibrary with the
    /// given R subprocess and immediately initializes it.
    ///
    /// # Arguments
    /// * `r_subprocess` - Optional R subprocess interface for querying package info
    ///
    /// # Returns
    /// An initialized PackageLibrary with lib_paths, base_packages, and base_exports populated
    ///
    /// Requirement 6.1, 6.2, 6.3, 7.1
    pub async fn new(r_subprocess: Option<RSubprocess>) -> Self {
        let mut lib = Self::with_subprocess(r_subprocess);
        if let Err(e) = lib.initialize().await {
            log::trace!("Failed to initialize PackageLibrary: {}", e);
            // Continue with empty/fallback state - the library is still usable
        }
        lib
    }

    /// Initialize the PackageLibrary with R subprocess query or fallback
    ///
    /// This method queries R for lib_paths and base_packages, falling back to
    /// hardcoded values if R is unavailable. It also pre-populates base_exports
    /// by querying exports for each base package.
    ///
    /// # Behavior
    /// 1. Query R for library paths using `.libPaths()`, or use platform-specific fallbacks
    /// 2. Query R for base packages using `.packages()`, or use hardcoded fallback list
    /// 3. For each base package, query its exports and combine into base_exports
    /// 4. If any query fails, gracefully fall back to defaults and continue
    ///
    /// Requirement 6.1: THE LSP SHALL query R subprocess at initialization to get
    /// the default search path using `.packages()`
    ///
    /// Requirement 6.2: IF R subprocess is unavailable at initialization, THE LSP
    /// SHALL use a hardcoded list of base packages
    ///
    /// Requirement 6.3: THE Base_Packages SHALL be available at all positions in
    /// all files without requiring explicit library() calls
    ///
    /// Requirement 7.1: THE LSP SHALL query R subprocess to get library paths
    /// using `.libPaths()`
    pub async fn initialize(&mut self) -> anyhow::Result<()> {
        // Step 1: Get library paths from R or use fallback
        let lib_paths = if let Some(ref r_subprocess) = self.r_subprocess {
            match r_subprocess.get_lib_paths().await {
                Ok(paths) => {
                    log::trace!("Got {} library paths from R", paths.len());
                    paths
                }
                Err(e) => {
                    log::trace!("Failed to get lib_paths from R: {}, using fallback", e);
                    crate::r_subprocess::get_fallback_lib_paths()
                }
            }
        } else {
            log::trace!("No R subprocess available, using fallback lib_paths");
            crate::r_subprocess::get_fallback_lib_paths()
        };
        self.lib_paths = lib_paths;

        // Step 2: Get base packages from R or use fallback
        let base_packages_list = if let Some(ref r_subprocess) = self.r_subprocess {
            match r_subprocess.get_base_packages().await {
                Ok(packages) => {
                    log::trace!(
                        "Got {} base packages from R: {:?}",
                        packages.len(),
                        packages
                    );
                    packages
                }
                Err(e) => {
                    log::trace!("Failed to get base_packages from R: {}, using fallback", e);
                    crate::r_subprocess::get_fallback_base_packages()
                }
            }
        } else {
            log::trace!("No R subprocess available, using fallback base_packages");
            crate::r_subprocess::get_fallback_base_packages()
        };
        self.base_packages = base_packages_list.iter().cloned().collect();

        // Step 3: Pre-populate base_exports from base packages
        // Query exports for each base package and combine them
        let mut all_base_exports = HashSet::new();

        for package in &base_packages_list {
            let exports = if let Some(ref r_subprocess) = self.r_subprocess {
                match r_subprocess.get_package_exports(package).await {
                    Ok(exports) => {
                        log::trace!(
                            "Got {} exports for base package '{}'",
                            exports.len(),
                            package
                        );
                        exports
                    }
                    Err(e) => {
                        log::trace!(
                            "Failed to get exports for base package '{}': {}, skipping",
                            package,
                            e
                        );
                        Vec::new()
                    }
                }
            } else {
                // No R subprocess - we can't get exports without it
                // This is expected when R is not available
                Vec::new()
            };

            // Add exports to the combined set
            for export in exports {
                all_base_exports.insert(export);
            }
        }

        log::trace!(
            "Initialized PackageLibrary with {} lib_paths, {} base_packages, {} base_exports",
            self.lib_paths.len(),
            self.base_packages.len(),
            all_base_exports.len()
        );

        self.base_exports = all_base_exports;

        Ok(())
    }

    /// Get package info, loading from cache or R subprocess
    ///
    /// This method checks the cache first, then queries R subprocess,
    /// falling back to NAMESPACE parsing if subprocess fails.
    ///
    /// # Behavior
    /// 1. Check cache first - return immediately if found
    /// 2. Try R subprocess to get exports and depends
    /// 3. If R subprocess fails, fall back to NAMESPACE/DESCRIPTION file parsing
    /// 4. Create PackageInfo and insert into cache
    /// 5. For meta-packages (tidyverse, tidymodels), attached_packages are set automatically
    ///
    /// Requirement 3.1: WHEN a package is loaded, THE Package_Resolver SHALL query R subprocess
    /// to get the package's exported symbols using `getNamespaceExports()`
    ///
    /// Requirement 3.2: IF R subprocess is unavailable, THE Package_Resolver SHALL fall back
    /// to parsing the package's NAMESPACE file directly
    ///
    /// Requirement 4.3: WHEN the package is `tidyverse`, THE Package_Resolver SHALL also load
    /// exports from: dplyr, readr, forcats, stringr, ggplot2, tibble, lubridate, tidyr, purrr
    ///
    /// Requirement 4.4: WHEN the package is `tidymodels`, THE Package_Resolver SHALL also load
    /// exports from the tidymodels packages
    pub async fn get_package(&self, name: &str) -> Option<Arc<PackageInfo>> {
        // Step 1: Check cache first
        if let Some(cached) = self.get_cached_package(name).await {
            log::trace!("Package '{}' found in cache", name);
            return Some(cached);
        }

        log::trace!("Package '{}' not in cache, attempting to load", name);

        // Step 2: Try R subprocess to get exports
        let (exports, depends) = if let Some(ref r_subprocess) = self.r_subprocess {
            // Try to get exports from R subprocess
            let exports_result = r_subprocess.get_package_exports(name).await;
            let depends_result = r_subprocess.get_package_depends(name).await;

            match exports_result {
                Ok(exports) => {
                    log::trace!(
                        "Got {} exports for package '{}' from R subprocess",
                        exports.len(),
                        name
                    );
                    let depends = depends_result.unwrap_or_else(|e| {
                        log::trace!(
                            "Failed to get depends for package '{}' from R: {}, using empty",
                            name,
                            e
                        );
                        Vec::new()
                    });
                    (exports, depends)
                }
                Err(e) => {
                    log::trace!(
                        "Failed to get exports for package '{}' from R subprocess: {}, falling back to NAMESPACE parsing",
                        name,
                        e
                    );
                    // Fall back to NAMESPACE parsing
                    self.load_package_from_filesystem(name)
                }
            }
        } else {
            // No R subprocess available, fall back to NAMESPACE parsing
            log::trace!(
                "No R subprocess available, falling back to NAMESPACE parsing for package '{}'",
                name
            );
            self.load_package_from_filesystem(name)
        };

        // If we couldn't get any exports, the package might not be installed
        if exports.is_empty() {
            log::trace!(
                "No exports found for package '{}', package may not be installed",
                name
            );
            // Still create a PackageInfo with empty exports - this allows us to cache
            // the fact that we tried to load this package and it had no exports
            // This prevents repeated failed lookups
        }

        // Step 3: Create PackageInfo and insert into cache
        // Note: PackageInfo::with_details automatically handles meta-packages
        // (tidyverse, tidymodels) by setting is_meta_package and attached_packages
        let exports_set: HashSet<String> = exports.into_iter().collect();
        let info = PackageInfo::with_details(name.to_string(), exports_set, depends, Vec::new());

        log::trace!(
            "Created PackageInfo for '{}': {} exports, {} depends, is_meta_package={}",
            name,
            info.exports.len(),
            info.depends.len(),
            info.is_meta_package
        );

        // Insert into cache
        self.insert_package(info).await;

        // Return the cached version
        self.get_cached_package(name).await
    }

    /// Load package exports and depends from filesystem (NAMESPACE/DESCRIPTION files)
    ///
    /// This is the fallback when R subprocess is unavailable.
    /// Searches lib_paths for the package directory and parses NAMESPACE/DESCRIPTION files.
    ///
    /// Requirement 3.2: IF R subprocess is unavailable, THE Package_Resolver SHALL fall back
    /// to parsing the package's NAMESPACE file directly
    fn load_package_from_filesystem(&self, name: &str) -> (Vec<String>, Vec<String>) {
        // Find the package directory in lib_paths
        let package_dir = self.find_package_directory(name);

        match package_dir {
            Some(dir) => {
                log::trace!("Found package '{}' at {:?}", name, dir);

                // Parse NAMESPACE file for exports
                let namespace_path = dir.join("NAMESPACE");
                let exports = if namespace_path.exists() {
                    match crate::namespace_parser::parse_namespace_exports(&namespace_path) {
                        Ok(exports) => {
                            // Filter out pattern markers - we can't expand patterns without R
                            let filtered: Vec<String> = exports
                                .into_iter()
                                .filter(|e| !e.starts_with("__PATTERN__:"))
                                .collect();
                            log::trace!(
                                "Parsed {} exports from NAMESPACE for package '{}'",
                                filtered.len(),
                                name
                            );
                            filtered
                        }
                        Err(e) => {
                            log::trace!("Failed to parse NAMESPACE for package '{}': {}", name, e);
                            Vec::new()
                        }
                    }
                } else {
                    log::trace!("No NAMESPACE file found for package '{}'", name);
                    Vec::new()
                };

                // Parse DESCRIPTION file for depends
                let description_path = dir.join("DESCRIPTION");
                let depends = if description_path.exists() {
                    match crate::namespace_parser::parse_description_depends(&description_path) {
                        Ok(depends) => {
                            log::trace!(
                                "Parsed {} depends from DESCRIPTION for package '{}'",
                                depends.len(),
                                name
                            );
                            depends
                        }
                        Err(e) => {
                            log::trace!(
                                "Failed to parse DESCRIPTION for package '{}': {}",
                                name,
                                e
                            );
                            Vec::new()
                        }
                    }
                } else {
                    log::trace!("No DESCRIPTION file found for package '{}'", name);
                    Vec::new()
                };

                (exports, depends)
            }
            None => {
                log::trace!(
                    "Package '{}' not found in any library path: {:?}",
                    name,
                    self.lib_paths
                );
                (Vec::new(), Vec::new())
            }
        }
    }

    /// Find the package directory in lib_paths
    ///
    /// Searches each library path for a directory with the package name.
    /// Returns the first match found.
    fn find_package_directory(&self, name: &str) -> Option<std::path::PathBuf> {
        for lib_path in &self.lib_paths {
            let package_dir = lib_path.join(name);
            if package_dir.is_dir() {
                return Some(package_dir);
            }
        }
        None
    }

    /// Check if a package exists (is installed)
    ///
    /// This is a synchronous method that checks if a package is installed by:
    /// 1. Checking if it's a base package (always available)
    /// 2. Checking if it's already in the cache
    /// 3. Checking if it exists on the filesystem in any lib_path
    ///
    /// This method does NOT load the package into cache - it only checks existence.
    /// Use `get_package()` to load and cache package information.
    ///
    /// **Validates: Requirement 15.1** - Used to detect non-installed packages for diagnostics
    pub fn package_exists(&self, name: &str) -> bool {
        // Base packages are always available
        if self.base_packages.contains(name) {
            return true;
        }

        // Check if already in cache (try_read to avoid blocking)
        if let Ok(cache) = self.packages.try_read() {
            if cache.contains_key(name) {
                return true;
            }
        }

        // Check if package directory exists on filesystem
        self.find_package_directory(name).is_some()
    }

    /// Get all exports for a package including Depends and attached packages
    ///
    /// This method loads the package and all its dependencies (from the Depends field
    /// and attached_packages for meta-packages), combining their exports into a single set.
    /// It tracks visited packages to handle circular dependencies.
    ///
    /// Results are cached in combined_exports for efficient repeated lookups.
    ///
    /// # Behavior
    /// 1. Check combined_exports cache first
    /// 2. Load the main package using `get_package()`
    /// 3. Add the package's exports to the result set
    /// 4. Recursively load all packages in `depends` and `attached_packages`
    /// 5. Track visited packages to prevent infinite loops from circular dependencies
    /// 6. Cache and return the combined exports from all packages
    ///
    /// Requirement 4.2: WHEN a package has dependencies in the `Depends` field,
    /// THE Package_Resolver SHALL also load exports from those packages at the same position
    ///
    /// Requirement 4.5: THE Package_Resolver SHALL handle circular dependencies in the
    /// `Depends` chain by tracking visited packages
    pub async fn get_all_exports(&self, name: &str) -> HashSet<String> {
        // Check cache first
        {
            let cache = self.combined_exports.read().await;
            if let Some(cached) = cache.get(name) {
                log::trace!("Using cached combined exports for package '{}'", name);
                return cached.as_ref().clone();
            }
        }

        // Compute exports
        let mut visited = HashSet::new();
        let mut all_exports = HashSet::new();
        self.collect_exports_recursive(name, &mut visited, &mut all_exports)
            .await;

        // Cache the result
        {
            let mut cache = self.combined_exports.write().await;
            cache.insert(name.to_string(), Arc::new(all_exports.clone()));
            log::trace!(
                "Cached {} combined exports for package '{}'",
                all_exports.len(),
                name
            );
        }

        all_exports
    }

    /// Helper method to recursively collect exports from a package and its dependencies
    ///
    /// This is a private helper for `get_all_exports()` that handles the recursive
    /// traversal of package dependencies while tracking visited packages to prevent
    /// infinite loops.
    ///
    /// # Arguments
    /// * `name` - The package name to load
    /// * `visited` - Set of already-visited package names (for cycle detection)
    /// * `all_exports` - Accumulator for all collected exports
    async fn collect_exports_recursive(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        all_exports: &mut HashSet<String>,
    ) {
        // Check if we've already visited this package (circular dependency detection)
        if visited.contains(name) {
            log::trace!(
                "Skipping already-visited package '{}' (circular dependency)",
                name
            );
            return;
        }

        // Mark this package as visited
        visited.insert(name.to_string());

        // Load the package
        let package_info = match self.get_package(name).await {
            Some(info) => info,
            None => {
                log::trace!(
                    "Could not load package '{}' for transitive dependency resolution",
                    name
                );
                return;
            }
        };

        // Add this package's exports to the result
        for export in &package_info.exports {
            all_exports.insert(export.clone());
        }

        log::trace!(
            "Added {} exports from package '{}' (total: {})",
            package_info.exports.len(),
            name,
            all_exports.len()
        );

        // Collect packages to process: depends + attached_packages (for meta-packages)
        let mut packages_to_process: Vec<String> = package_info.depends.clone();

        // For meta-packages (tidyverse, tidymodels), also process attached packages
        if package_info.is_meta_package {
            for attached in &package_info.attached_packages {
                if !packages_to_process.contains(attached) {
                    packages_to_process.push(attached.clone());
                }
            }
        }

        // Recursively process all dependency packages
        // Use Box::pin for recursive async calls
        for dep_name in packages_to_process {
            Box::pin(self.collect_exports_recursive(&dep_name, visited, all_exports)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_info_new() {
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());
        exports.insert("func2".to_string());

        let info = PackageInfo::new("testpkg".to_string(), exports.clone());

        assert_eq!(info.name, "testpkg");
        assert_eq!(info.exports, exports);
        assert!(info.depends.is_empty());
        assert!(!info.is_meta_package);
        assert!(info.attached_packages.is_empty());
        assert!(info.lazy_data.is_empty());
    }

    #[test]
    fn test_package_info_tidyverse_is_meta_package() {
        let info = PackageInfo::new("tidyverse".to_string(), HashSet::new());

        assert!(info.is_meta_package);
        assert!(!info.attached_packages.is_empty());
        assert!(info.attached_packages.contains(&"dplyr".to_string()));
        assert!(info.attached_packages.contains(&"ggplot2".to_string()));
        assert!(info.attached_packages.contains(&"tidyr".to_string()));
    }

    #[test]
    fn test_package_info_tidymodels_is_meta_package() {
        let info = PackageInfo::new("tidymodels".to_string(), HashSet::new());

        assert!(info.is_meta_package);
        assert!(!info.attached_packages.is_empty());
        assert!(info.attached_packages.contains(&"parsnip".to_string()));
        assert!(info.attached_packages.contains(&"recipes".to_string()));
        assert!(info.attached_packages.contains(&"yardstick".to_string()));
    }

    #[test]
    fn test_package_info_with_details() {
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());

        let depends = vec!["dep1".to_string(), "dep2".to_string()];
        let lazy_data = vec!["dataset1".to_string()];

        let info = PackageInfo::with_details(
            "testpkg".to_string(),
            exports.clone(),
            depends.clone(),
            lazy_data.clone(),
        );

        assert_eq!(info.name, "testpkg");
        assert_eq!(info.exports, exports);
        assert_eq!(info.depends, depends);
        assert_eq!(info.lazy_data, lazy_data);
        assert!(!info.is_meta_package);
    }

    #[tokio::test]
    async fn test_package_library_new_empty() {
        let lib = PackageLibrary::new_empty();

        assert!(lib.lib_paths().is_empty());
        assert!(lib.base_packages().is_empty());
        assert!(lib.base_exports().is_empty());
        assert_eq!(lib.cached_count().await, 0);
    }

    #[tokio::test]
    async fn test_package_library_insert_and_get() {
        let lib = PackageLibrary::new_empty();

        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());

        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        assert!(lib.is_cached("dplyr").await);
        assert!(!lib.is_cached("ggplot2").await);

        let cached = lib.get_cached_package("dplyr").await;
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.name, "dplyr");
        assert!(cached.exports.contains("mutate"));
        assert!(cached.exports.contains("filter"));
    }

    #[tokio::test]
    async fn test_package_library_invalidate() {
        let lib = PackageLibrary::new_empty();

        let info = PackageInfo::new("testpkg".to_string(), HashSet::new());
        lib.insert_package(info).await;

        assert!(lib.is_cached("testpkg").await);

        lib.invalidate("testpkg").await;

        assert!(!lib.is_cached("testpkg").await);
    }

    #[tokio::test]
    async fn test_package_library_clear_cache() {
        let lib = PackageLibrary::new_empty();

        lib.insert_package(PackageInfo::new("pkg1".to_string(), HashSet::new()))
            .await;
        lib.insert_package(PackageInfo::new("pkg2".to_string(), HashSet::new()))
            .await;

        assert_eq!(lib.cached_count().await, 2);

        lib.clear_cache().await;

        assert_eq!(lib.cached_count().await, 0);
    }

    #[tokio::test]
    async fn test_package_library_is_package_export() {
        let lib = PackageLibrary::new_empty();

        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());

        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        assert!(lib.is_package_export("mutate", "dplyr").await);
        assert!(lib.is_package_export("filter", "dplyr").await);
        assert!(!lib.is_package_export("ggplot", "dplyr").await);
        assert!(!lib.is_package_export("mutate", "ggplot2").await);
    }

    #[test]
    fn test_package_library_is_base_export() {
        let mut lib = PackageLibrary::new_empty();

        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());
        base_exports.insert("cat".to_string());
        base_exports.insert("sum".to_string());

        lib.set_base_exports(base_exports);

        assert!(lib.is_base_export("print"));
        assert!(lib.is_base_export("cat"));
        assert!(lib.is_base_export("sum"));
        assert!(!lib.is_base_export("mutate"));
    }

    #[test]
    fn test_package_library_is_base_package() {
        let mut lib = PackageLibrary::new_empty();

        let mut base_packages = HashSet::new();
        base_packages.insert("base".to_string());
        base_packages.insert("stats".to_string());
        base_packages.insert("utils".to_string());

        lib.set_base_packages(base_packages);

        assert!(lib.is_base_package("base"));
        assert!(lib.is_base_package("stats"));
        assert!(lib.is_base_package("utils"));
        assert!(!lib.is_base_package("dplyr"));
    }

    #[test]
    fn test_tidyverse_packages_constant() {
        // Verify the tidyverse packages list matches the requirement
        assert!(TIDYVERSE_PACKAGES.contains(&"dplyr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"readr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"forcats"));
        assert!(TIDYVERSE_PACKAGES.contains(&"stringr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"ggplot2"));
        assert!(TIDYVERSE_PACKAGES.contains(&"tibble"));
        assert!(TIDYVERSE_PACKAGES.contains(&"lubridate"));
        assert!(TIDYVERSE_PACKAGES.contains(&"tidyr"));
        assert!(TIDYVERSE_PACKAGES.contains(&"purrr"));
        assert_eq!(TIDYVERSE_PACKAGES.len(), 9);
    }

    #[test]
    fn test_tidymodels_packages_constant() {
        // Verify the tidymodels packages list matches the requirement
        assert!(TIDYMODELS_PACKAGES.contains(&"broom"));
        assert!(TIDYMODELS_PACKAGES.contains(&"dials"));
        assert!(TIDYMODELS_PACKAGES.contains(&"dplyr"));
        assert!(TIDYMODELS_PACKAGES.contains(&"ggplot2"));
        assert!(TIDYMODELS_PACKAGES.contains(&"infer"));
        assert!(TIDYMODELS_PACKAGES.contains(&"modeldata"));
        assert!(TIDYMODELS_PACKAGES.contains(&"parsnip"));
        assert!(TIDYMODELS_PACKAGES.contains(&"purrr"));
        assert!(TIDYMODELS_PACKAGES.contains(&"recipes"));
        assert!(TIDYMODELS_PACKAGES.contains(&"rsample"));
        assert!(TIDYMODELS_PACKAGES.contains(&"tibble"));
        assert!(TIDYMODELS_PACKAGES.contains(&"tidyr"));
        assert!(TIDYMODELS_PACKAGES.contains(&"tune"));
        assert!(TIDYMODELS_PACKAGES.contains(&"workflows"));
        assert!(TIDYMODELS_PACKAGES.contains(&"workflowsets"));
        assert!(TIDYMODELS_PACKAGES.contains(&"yardstick"));
        assert_eq!(TIDYMODELS_PACKAGES.len(), 16);
    }

    #[tokio::test]
    async fn test_concurrent_read_access() {
        // Test that multiple readers can access the cache concurrently
        // This validates Requirement 13.4
        let lib = Arc::new(PackageLibrary::new_empty());

        // Insert some packages
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());
        lib.insert_package(PackageInfo::new("pkg1".to_string(), exports.clone()))
            .await;
        lib.insert_package(PackageInfo::new("pkg2".to_string(), exports))
            .await;

        // Spawn multiple concurrent readers
        let mut handles = Vec::new();
        for i in 0..10 {
            let lib_clone = Arc::clone(&lib);
            let handle = tokio::spawn(async move {
                let pkg_name = if i % 2 == 0 { "pkg1" } else { "pkg2" };
                let cached = lib_clone.get_cached_package(pkg_name).await;
                assert!(cached.is_some());
                cached.unwrap().name.clone()
            });
            handles.push(handle);
        }

        // Wait for all readers to complete
        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok());
        }
    }

    // Tests for initialize() - Task 3.2

    #[tokio::test]
    async fn test_initialize_without_r_subprocess_uses_fallback() {
        // Test that initialize() uses fallback values when R subprocess is None
        // Requirement 6.2: IF R subprocess is unavailable, use hardcoded base packages
        let mut lib = PackageLibrary::with_subprocess(None);
        let result = lib.initialize().await;

        assert!(result.is_ok(), "initialize() should succeed even without R");

        // Should have fallback base packages
        let fallback_packages = crate::r_subprocess::get_fallback_base_packages();
        for pkg in &fallback_packages {
            assert!(
                lib.is_base_package(pkg),
                "Should have fallback base package '{}'",
                pkg
            );
        }

        // base_exports will be empty without R subprocess (can't query exports)
        // This is expected behavior - we can't get exports without R
    }

    #[tokio::test]
    async fn test_initialize_with_r_subprocess() {
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let mut lib = PackageLibrary::with_subprocess(Some(r_subprocess));
        let result = lib.initialize().await;

        assert!(result.is_ok(), "initialize() should succeed with R");

        // Should have library paths
        assert!(
            !lib.lib_paths().is_empty(),
            "Should have at least one library path"
        );

        // Should have base packages
        assert!(!lib.base_packages().is_empty(), "Should have base packages");
        assert!(lib.is_base_package("base"), "Should have 'base' package");
        assert!(lib.is_base_package("stats"), "Should have 'stats' package");
        assert!(lib.is_base_package("utils"), "Should have 'utils' package");

        // Should have base exports (from querying base packages)
        assert!(!lib.base_exports().is_empty(), "Should have base exports");
        // Common base functions should be in exports
        assert!(
            lib.is_base_export("print"),
            "Should have 'print' in base exports"
        );
        assert!(
            lib.is_base_export("cat"),
            "Should have 'cat' in base exports"
        );
    }

    #[tokio::test]
    async fn test_new_constructor_initializes() {
        // Test the convenience new() constructor
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Should be initialized
        assert!(
            !lib.lib_paths().is_empty(),
            "new() should initialize lib_paths"
        );
        assert!(
            !lib.base_packages().is_empty(),
            "new() should initialize base_packages"
        );
        assert!(
            !lib.base_exports().is_empty(),
            "new() should initialize base_exports"
        );
    }

    #[tokio::test]
    async fn test_new_constructor_without_r() {
        // Test new() constructor without R subprocess
        let lib = PackageLibrary::new(None).await;

        // Should have fallback base packages
        let fallback_packages = crate::r_subprocess::get_fallback_base_packages();
        for pkg in &fallback_packages {
            assert!(
                lib.is_base_package(pkg),
                "new(None) should have fallback base package '{}'",
                pkg
            );
        }
    }

    #[tokio::test]
    async fn test_initialize_base_exports_contain_common_functions() {
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Requirement 6.3: Base packages shall be available at all positions
        // These are common functions that should be in base exports
        let common_base_functions = [
            "print", "cat", "c", "list", "length", "sum", "mean", "paste", "paste0", "sprintf",
            "format",
        ];

        for func in &common_base_functions {
            assert!(
                lib.is_base_export(func),
                "Base exports should contain common function '{}'",
                func
            );
        }
    }

    #[tokio::test]
    async fn test_initialize_lib_paths_exist() {
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // All returned lib_paths should exist
        for path in lib.lib_paths() {
            assert!(path.exists(), "Library path {:?} should exist", path);
        }
    }

    #[tokio::test]
    async fn test_initialize_idempotent() {
        // Test that calling initialize() multiple times is safe
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let mut lib = PackageLibrary::with_subprocess(Some(r_subprocess));

        // Initialize twice
        let result1 = lib.initialize().await;
        let base_packages_count = lib.base_packages().len();
        let base_exports_count = lib.base_exports().len();

        let result2 = lib.initialize().await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());

        // Should have same results after second initialization
        assert_eq!(
            lib.base_packages().len(),
            base_packages_count,
            "Base packages count should be same after re-initialization"
        );
        assert_eq!(
            lib.base_exports().len(),
            base_exports_count,
            "Base exports count should be same after re-initialization"
        );
    }

    // ============================================================================
    // Tests for get_package() - Task 3.3
    // ============================================================================

    #[tokio::test]
    async fn test_get_package_returns_cached() {
        // Test that get_package returns cached package if available
        let lib = PackageLibrary::new_empty();

        // Pre-populate cache
        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());
        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        // get_package should return the cached version
        let result = lib.get_package("dplyr").await;
        assert!(result.is_some());
        let pkg = result.unwrap();
        assert_eq!(pkg.name, "dplyr");
        assert!(pkg.exports.contains("mutate"));
        assert!(pkg.exports.contains("filter"));
    }

    #[tokio::test]
    async fn test_get_package_with_r_subprocess() {
        // Test that get_package queries R subprocess for non-cached packages
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Query a base package that should be installed
        let result = lib.get_package("base").await;
        assert!(result.is_some(), "Should be able to load 'base' package");
        let pkg = result.unwrap();
        assert_eq!(pkg.name, "base");
        assert!(!pkg.exports.is_empty(), "base package should have exports");

        // Common base functions should be in exports
        assert!(pkg.exports.contains("print"), "base should export 'print'");
        assert!(pkg.exports.contains("cat"), "base should export 'cat'");
        assert!(pkg.exports.contains("c"), "base should export 'c'");
    }

    #[tokio::test]
    async fn test_get_package_caches_result() {
        // Test that get_package caches the result after loading
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // First call should load and cache
        assert!(!lib.is_cached("stats").await);
        let result1 = lib.get_package("stats").await;
        assert!(result1.is_some());
        assert!(lib.is_cached("stats").await);

        // Second call should return cached version
        let result2 = lib.get_package("stats").await;
        assert!(result2.is_some());

        // Both should be the same Arc
        assert!(Arc::ptr_eq(&result1.unwrap(), &result2.unwrap()));
    }

    #[tokio::test]
    async fn test_get_package_tidyverse_is_meta_package() {
        // Test that tidyverse is recognized as a meta-package
        // Skip if R is not available or tidyverse is not installed
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.get_package("tidyverse").await;
        // tidyverse may or may not be installed, so we check if it was loaded
        if let Some(pkg) = result {
            assert!(pkg.is_meta_package, "tidyverse should be a meta-package");
            assert!(
                !pkg.attached_packages.is_empty(),
                "tidyverse should have attached packages"
            );
            assert!(
                pkg.attached_packages.contains(&"dplyr".to_string()),
                "tidyverse should attach dplyr"
            );
            assert!(
                pkg.attached_packages.contains(&"ggplot2".to_string()),
                "tidyverse should attach ggplot2"
            );
        }
    }

    #[tokio::test]
    async fn test_get_package_tidymodels_is_meta_package() {
        // Test that tidymodels is recognized as a meta-package
        // Skip if R is not available or tidymodels is not installed
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.get_package("tidymodels").await;
        // tidymodels may or may not be installed, so we check if it was loaded
        if let Some(pkg) = result {
            assert!(pkg.is_meta_package, "tidymodels should be a meta-package");
            assert!(
                !pkg.attached_packages.is_empty(),
                "tidymodels should have attached packages"
            );
            assert!(
                pkg.attached_packages.contains(&"parsnip".to_string()),
                "tidymodels should attach parsnip"
            );
            assert!(
                pkg.attached_packages.contains(&"recipes".to_string()),
                "tidymodels should attach recipes"
            );
        }
    }

    #[tokio::test]
    async fn test_get_package_nonexistent_returns_empty_exports() {
        // Test that get_package handles non-existent packages gracefully
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Query a package that definitely doesn't exist
        let result = lib.get_package("__nonexistent_package_xyz__").await;
        // Should return Some with empty exports (cached negative result)
        assert!(result.is_some());
        let pkg = result.unwrap();
        assert!(pkg.exports.is_empty());
    }

    #[tokio::test]
    async fn test_get_package_without_r_subprocess() {
        // Test that get_package works without R subprocess (filesystem fallback)
        let lib = PackageLibrary::new(None).await;

        // Without R subprocess, we rely on fallback lib_paths
        // The method should not panic regardless of whether packages are found
        let result = lib.get_package("dplyr").await;
        // Should return Some (we cache the result either way)
        assert!(result.is_some());
        // Note: exports may or may not be empty depending on whether
        // fallback lib_paths contain the package
    }

    #[tokio::test]
    async fn test_get_package_regular_package_not_meta() {
        // Test that regular packages are not marked as meta-packages
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.get_package("stats").await;
        assert!(result.is_some());
        let pkg = result.unwrap();
        assert!(!pkg.is_meta_package, "stats should not be a meta-package");
        assert!(
            pkg.attached_packages.is_empty(),
            "stats should not have attached packages"
        );
    }

    #[tokio::test]
    async fn test_get_package_loads_depends() {
        // Test that get_package loads the Depends field
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // stats package typically depends on base packages
        let result = lib.get_package("stats").await;
        assert!(result.is_some());
        // Note: depends may be empty for base packages, that's OK
    }

    #[tokio::test]
    async fn test_find_package_directory() {
        // Test the find_package_directory helper
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // stats package should be findable and has standard structure
        let stats_dir = lib.find_package_directory("stats");
        assert!(stats_dir.is_some(), "Should find 'stats' package directory");
        let stats_dir = stats_dir.unwrap();
        assert!(stats_dir.is_dir());
        // stats package should have DESCRIPTION (all packages have this)
        assert!(stats_dir.join("DESCRIPTION").exists());
    }

    #[tokio::test]
    async fn test_find_package_directory_nonexistent() {
        // Test that find_package_directory returns None for non-existent packages
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        let result = lib.find_package_directory("__nonexistent_package_xyz__");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_load_package_from_filesystem() {
        // Test the filesystem fallback loading
        // Skip if R is not available (we need lib_paths)
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Load stats package from filesystem (it has a standard NAMESPACE)
        let (exports, _depends) = lib.load_package_from_filesystem("stats");

        // stats package should have exports from NAMESPACE
        assert!(
            !exports.is_empty(),
            "stats should have exports from NAMESPACE"
        );
    }

    // ============================================================================
    // Tests for get_all_exports() - Task 3.4
    // ============================================================================

    #[tokio::test]
    async fn test_get_all_exports_single_package() {
        // Test that get_all_exports returns exports for a single package with no deps
        let lib = PackageLibrary::new_empty();

        // Create a package with no dependencies
        let mut exports = HashSet::new();
        exports.insert("func1".to_string());
        exports.insert("func2".to_string());
        let info = PackageInfo::new("testpkg".to_string(), exports.clone());
        lib.insert_package(info).await;

        let all_exports = lib.get_all_exports("testpkg").await;

        assert_eq!(all_exports.len(), 2);
        assert!(all_exports.contains("func1"));
        assert!(all_exports.contains("func2"));
    }

    #[tokio::test]
    async fn test_get_all_exports_with_depends() {
        // Test that get_all_exports includes exports from Depends packages
        // Requirement 4.2
        let lib = PackageLibrary::new_empty();

        // Create dependency package
        let mut dep_exports = HashSet::new();
        dep_exports.insert("dep_func".to_string());
        let dep_info = PackageInfo::new("deppkg".to_string(), dep_exports);
        lib.insert_package(dep_info).await;

        // Create main package that depends on deppkg
        let mut main_exports = HashSet::new();
        main_exports.insert("main_func".to_string());
        let main_info = PackageInfo::with_details(
            "mainpkg".to_string(),
            main_exports,
            vec!["deppkg".to_string()],
            Vec::new(),
        );
        lib.insert_package(main_info).await;

        let all_exports = lib.get_all_exports("mainpkg").await;

        // Should have exports from both packages
        assert!(
            all_exports.contains("main_func"),
            "Should have main package export"
        );
        assert!(
            all_exports.contains("dep_func"),
            "Should have dependency export"
        );
        assert_eq!(all_exports.len(), 2);
    }

    #[tokio::test]
    async fn test_get_all_exports_transitive_depends() {
        // Test that get_all_exports handles transitive dependencies (A -> B -> C)
        let lib = PackageLibrary::new_empty();

        // Create package C (no deps)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::new("pkgC".to_string(), c_exports);
        lib.insert_package(c_info).await;

        // Create package B (depends on C)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Create package A (depends on B)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let all_exports = lib.get_all_exports("pkgA").await;

        // Should have exports from all three packages
        assert!(all_exports.contains("a_func"), "Should have A's export");
        assert!(all_exports.contains("b_func"), "Should have B's export");
        assert!(all_exports.contains("c_func"), "Should have C's export");
        assert_eq!(all_exports.len(), 3);
    }

    #[tokio::test]
    async fn test_get_all_exports_circular_dependency() {
        // Test that get_all_exports handles circular dependencies
        // Requirement 4.5
        let lib = PackageLibrary::new_empty();

        // Create package A (depends on B)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        // Create package B (depends on A - circular!)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgA".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Should not hang or panic - should terminate and return exports
        let all_exports = lib.get_all_exports("pkgA").await;

        // Should have exports from both packages
        assert!(all_exports.contains("a_func"), "Should have A's export");
        assert!(all_exports.contains("b_func"), "Should have B's export");
        assert_eq!(all_exports.len(), 2);
    }

    #[tokio::test]
    async fn test_get_all_exports_self_dependency() {
        // Test that get_all_exports handles self-referential dependencies
        let lib = PackageLibrary::new_empty();

        // Create package that depends on itself (pathological case)
        let mut exports = HashSet::new();
        exports.insert("func".to_string());
        let info = PackageInfo::with_details(
            "selfpkg".to_string(),
            exports,
            vec!["selfpkg".to_string()],
            Vec::new(),
        );
        lib.insert_package(info).await;

        // Should not hang or panic
        let all_exports = lib.get_all_exports("selfpkg").await;

        assert!(all_exports.contains("func"));
        assert_eq!(all_exports.len(), 1);
    }

    #[tokio::test]
    async fn test_get_all_exports_meta_package_attached() {
        // Test that get_all_exports includes attached packages for meta-packages
        let lib = PackageLibrary::new_empty();

        // Create some packages that tidyverse would attach
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        dplyr_exports.insert("filter".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        let mut ggplot2_exports = HashSet::new();
        ggplot2_exports.insert("ggplot".to_string());
        ggplot2_exports.insert("aes".to_string());
        let ggplot2_info = PackageInfo::new("ggplot2".to_string(), ggplot2_exports);
        lib.insert_package(ggplot2_info).await;

        // Create tidyverse (meta-package)
        let mut tidyverse_exports = HashSet::new();
        tidyverse_exports.insert("tidyverse_conflicts".to_string());
        let tidyverse_info = PackageInfo::new("tidyverse".to_string(), tidyverse_exports);
        lib.insert_package(tidyverse_info).await;

        let all_exports = lib.get_all_exports("tidyverse").await;

        // Should have tidyverse's own exports
        assert!(all_exports.contains("tidyverse_conflicts"));
        // Should have dplyr exports (attached package)
        assert!(all_exports.contains("mutate"));
        assert!(all_exports.contains("filter"));
        // Should have ggplot2 exports (attached package)
        assert!(all_exports.contains("ggplot"));
        assert!(all_exports.contains("aes"));
    }

    #[tokio::test]
    async fn test_get_all_exports_missing_dependency() {
        // Test that get_all_exports handles missing dependencies gracefully
        let lib = PackageLibrary::new_empty();

        // Create package that depends on a non-existent package
        let mut exports = HashSet::new();
        exports.insert("func".to_string());
        let info = PackageInfo::with_details(
            "mainpkg".to_string(),
            exports,
            vec!["nonexistent".to_string()],
            Vec::new(),
        );
        lib.insert_package(info).await;

        // Should not panic, should return main package's exports
        let all_exports = lib.get_all_exports("mainpkg").await;

        assert!(all_exports.contains("func"));
        // nonexistent package contributes nothing (empty exports)
    }

    #[tokio::test]
    async fn test_get_all_exports_nonexistent_package() {
        // Test that get_all_exports returns empty set for non-existent package
        let lib = PackageLibrary::new_empty();

        let all_exports = lib.get_all_exports("nonexistent").await;

        assert!(all_exports.is_empty());
    }

    #[tokio::test]
    async fn test_get_all_exports_diamond_dependency() {
        // Test diamond dependency pattern: A -> B, A -> C, B -> D, C -> D
        let lib = PackageLibrary::new_empty();

        // Create package D (shared dependency)
        let mut d_exports = HashSet::new();
        d_exports.insert("d_func".to_string());
        let d_info = PackageInfo::new("pkgD".to_string(), d_exports);
        lib.insert_package(d_info).await;

        // Create package B (depends on D)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgD".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Create package C (depends on D)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::with_details(
            "pkgC".to_string(),
            c_exports,
            vec!["pkgD".to_string()],
            Vec::new(),
        );
        lib.insert_package(c_info).await;

        // Create package A (depends on B and C)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let all_exports = lib.get_all_exports("pkgA").await;

        // Should have exports from all four packages (D only counted once)
        assert!(all_exports.contains("a_func"));
        assert!(all_exports.contains("b_func"));
        assert!(all_exports.contains("c_func"));
        assert!(all_exports.contains("d_func"));
        assert_eq!(all_exports.len(), 4);
    }

    #[tokio::test]
    async fn test_get_all_exports_with_r_subprocess() {
        // Integration test with real R subprocess
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // stats package typically depends on base packages
        let all_exports = lib.get_all_exports("stats").await;

        // Should have stats exports
        assert!(!all_exports.is_empty(), "stats should have exports");
        // Common stats functions
        assert!(
            all_exports.contains("lm")
                || all_exports.contains("t.test")
                || all_exports.contains("sd"),
            "stats should have common statistical functions"
        );
    }

    // ============================================================================
    // Property-Based Tests for Cache Idempotence - Task 3.6
    // ============================================================================

    use proptest::prelude::*;

    /// Strategy to generate valid R package names
    ///
    /// R package names must:
    /// - Start with a letter
    /// - Contain only letters, digits, and dots
    /// - Not end with a dot
    /// - Be at least 2 characters long
    fn package_name_strategy() -> impl Strategy<Value = String> {
        // Generate package names that look like real R packages
        prop::string::string_regex("[a-z][a-z0-9]{1,9}")
            .unwrap()
            .prop_filter("Package name must be at least 2 chars", |s| s.len() >= 2)
    }

    /// Strategy to generate a set of export names
    fn exports_strategy() -> impl Strategy<Value = HashSet<String>> {
        prop::collection::hash_set(
            prop::string::string_regex("[a-z][a-zA-Z0-9_.]{0,15}").unwrap(),
            0..=20,
        )
    }

    /// Strategy to generate a list of dependency package names
    fn depends_strategy() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(package_name_strategy(), 0..=5)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15a: For any package P, repeated calls to get_package(P) SHALL
        // return identical results (same Arc pointer after caching).
        #[test]
        fn prop_cache_idempotence_same_arc(
            pkg_name in package_name_strategy(),
            exports in exports_strategy()
        ) {
            // Use tokio runtime for async test
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache with a package
                let info = PackageInfo::new(pkg_name.clone(), exports.clone());
                lib.insert_package(info).await;

                // Call get_package multiple times
                let result1 = lib.get_package(&pkg_name).await;
                let result2 = lib.get_package(&pkg_name).await;
                let result3 = lib.get_package(&pkg_name).await;

                // All results should be Some
                prop_assert!(result1.is_some(), "First call should return Some");
                prop_assert!(result2.is_some(), "Second call should return Some");
                prop_assert!(result3.is_some(), "Third call should return Some");

                let arc1 = result1.unwrap();
                let arc2 = result2.unwrap();
                let arc3 = result3.unwrap();

                // All should point to the same Arc (cache consistency)
                prop_assert!(
                    Arc::ptr_eq(&arc1, &arc2),
                    "First and second calls should return same Arc"
                );
                prop_assert!(
                    Arc::ptr_eq(&arc2, &arc3),
                    "Second and third calls should return same Arc"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15b: For any package P, the exports returned by repeated calls
        // to get_package(P) SHALL be identical sets.
        #[test]
        fn prop_cache_idempotence_same_exports(
            pkg_name in package_name_strategy(),
            exports in exports_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache
                let info = PackageInfo::new(pkg_name.clone(), exports.clone());
                lib.insert_package(info).await;

                // Call get_package multiple times
                let result1 = lib.get_package(&pkg_name).await;
                let result2 = lib.get_package(&pkg_name).await;

                let pkg1 = result1.unwrap();
                let pkg2 = result2.unwrap();

                // Exports should be identical
                prop_assert_eq!(
                    &pkg1.exports,
                    &pkg2.exports,
                    "Exports should be identical across calls"
                );

                // Exports should match what we inserted
                prop_assert_eq!(
                    &pkg1.exports,
                    &exports,
                    "Exports should match original inserted exports"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15c: For any package P with dependencies, repeated calls to
        // get_all_exports(P) SHALL return identical export sets.
        #[test]
        fn prop_cache_idempotence_all_exports(
            pkg_name in package_name_strategy(),
            exports in exports_strategy(),
            depends in depends_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache with main package and its dependencies
                let info = PackageInfo::with_details(
                    pkg_name.clone(),
                    exports.clone(),
                    depends.clone(),
                    Vec::new(),
                );
                lib.insert_package(info).await;

                // Also insert dependency packages (with empty exports for simplicity)
                for dep in &depends {
                    if !lib.is_cached(dep).await {
                        let dep_info = PackageInfo::new(dep.clone(), HashSet::new());
                        lib.insert_package(dep_info).await;
                    }
                }

                // Call get_all_exports multiple times
                let result1 = lib.get_all_exports(&pkg_name).await;
                let result2 = lib.get_all_exports(&pkg_name).await;
                let result3 = lib.get_all_exports(&pkg_name).await;

                // All results should be identical
                prop_assert_eq!(
                    &result1,
                    &result2,
                    "First and second get_all_exports should return identical sets"
                );
                prop_assert_eq!(
                    &result2,
                    &result3,
                    "Second and third get_all_exports should return identical sets"
                );

                // Results should contain the original exports
                for export in &exports {
                    prop_assert!(
                        result1.contains(export),
                        "Result should contain original export '{}'",
                        export
                    );
                }

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15d: Cache SHALL be populated after first get_package call,
        // and is_cached SHALL return true for subsequent checks.
        #[test]
        fn prop_cache_populated_after_first_call(
            pkg_name in package_name_strategy(),
            exports in exports_strategy()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Pre-populate cache (simulating what get_package does internally)
                let info = PackageInfo::new(pkg_name.clone(), exports);
                lib.insert_package(info).await;

                // Verify cache is populated
                prop_assert!(
                    lib.is_cached(&pkg_name).await,
                    "Package should be cached after insertion"
                );

                // Call get_package
                let result = lib.get_package(&pkg_name).await;
                prop_assert!(result.is_some(), "get_package should return Some");

                // Cache should still be populated
                prop_assert!(
                    lib.is_cached(&pkg_name).await,
                    "Package should still be cached after get_package"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 15: Cache Idempotence
        // **Validates: Requirements 3.7, 13.1, 13.2**
        //
        // Property 15e: Multiple packages can be cached independently, and
        // accessing one SHALL NOT affect the cached state of others.
        #[test]
        fn prop_cache_independence(
            pkg1_name in package_name_strategy(),
            pkg2_name in package_name_strategy(),
            exports1 in exports_strategy(),
            exports2 in exports_strategy()
        ) {
            // Skip if package names are the same
            prop_assume!(pkg1_name != pkg2_name);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert both packages
                let info1 = PackageInfo::new(pkg1_name.clone(), exports1.clone());
                let info2 = PackageInfo::new(pkg2_name.clone(), exports2.clone());
                lib.insert_package(info1).await;
                lib.insert_package(info2).await;

                // Access pkg1 multiple times
                let result1a = lib.get_package(&pkg1_name).await;
                let result1b = lib.get_package(&pkg1_name).await;

                // Access pkg2
                let result2 = lib.get_package(&pkg2_name).await;

                // Access pkg1 again
                let result1c = lib.get_package(&pkg1_name).await;

                // All pkg1 results should be the same Arc
                let arc1a = result1a.unwrap();
                let arc1b = result1b.unwrap();
                let arc1c = result1c.unwrap();
                prop_assert!(Arc::ptr_eq(&arc1a, &arc1b));
                prop_assert!(Arc::ptr_eq(&arc1b, &arc1c));

                // pkg2 should have its own Arc
                let arc2 = result2.unwrap();
                prop_assert!(!Arc::ptr_eq(&arc1a, &arc2), "Different packages should have different Arcs");

                Ok(())
            })?;
        }
    }

    // ============================================================================
    // Property-Based Tests for Circular Dependency Handling - Task 3.7
    // ============================================================================

    /// Strategy to generate a dependency graph with guaranteed cycles
    ///
    /// Generates a set of packages where all packages are connected in a single cycle.
    /// Returns a Vec of (name, exports, depends) tuples.
    fn circular_deps_strategy() -> impl Strategy<Value = Vec<(String, HashSet<String>, Vec<String>)>>
    {
        // Generate 2-6 packages with circular dependencies
        (2..=6usize).prop_flat_map(|num_packages| {
            // Generate unique package names
            let names = prop::collection::vec(
                prop::string::string_regex("[a-z][a-z0-9]{1,5}").unwrap(),
                num_packages,
            );

            // Generate exports for each package
            let exports = prop::collection::vec(
                prop::collection::hash_set(
                    prop::string::string_regex("[a-z][a-zA-Z0-9_]{0,10}").unwrap(),
                    1..=5,
                ),
                num_packages,
            );

            (names, exports).prop_map(|(mut names, exports)| {
                // Ensure unique names by appending index if needed
                let mut seen = std::collections::HashSet::new();
                for (i, name) in names.iter_mut().enumerate() {
                    while seen.contains(name) {
                        name.push_str(&format!("{}", i));
                    }
                    seen.insert(name.clone());
                }

                let mut packages: Vec<(String, HashSet<String>, Vec<String>)> = Vec::new();

                // Create all packages first with empty dependencies
                for (name, exp) in names.into_iter().zip(exports.into_iter()) {
                    packages.push((name, exp, Vec::new()));
                }

                // Create a single cycle connecting all packages: A -> B -> C -> ... -> A
                // This ensures all packages are in the same cycle
                for i in 0..packages.len() {
                    let next_idx = (i + 1) % packages.len();
                    packages[i].2 = vec![packages[next_idx].0.clone()];
                }

                packages
            })
        })
    }

    /// Strategy to generate self-referential dependency (package depends on itself)
    fn self_dependency_strategy() -> impl Strategy<Value = (String, HashSet<String>)> {
        (
            prop::string::string_regex("[a-z][a-z0-9]{1,5}").unwrap(),
            prop::collection::hash_set(
                prop::string::string_regex("[a-z][a-zA-Z0-9_]{0,10}").unwrap(),
                1..=5,
            ),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7a: For any set of packages with circular dependencies in their
        // Depends fields, the Package_Resolver SHALL terminate and return exports
        // without infinite loops.
        #[test]
        fn prop_circular_dependency_terminates(packages in circular_deps_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                // Call get_all_exports on the first package
                // This should terminate without hanging (the test itself acts as a timeout)
                if let Some((first_name, _, _)) = packages.first() {
                    let result = lib.get_all_exports(first_name).await;

                    // The function should return a non-empty result (at least the first package's exports)
                    prop_assert!(
                        !result.is_empty() || packages.iter().all(|(_, e, _)| e.is_empty()),
                        "get_all_exports should return exports from the dependency graph"
                    );
                }

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7b: For any set of packages with circular dependencies,
        // get_all_exports SHALL return exports from ALL packages in the cycle.
        #[test]
        fn prop_circular_dependency_returns_all_exports(packages in circular_deps_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Collect all expected exports
                let mut all_expected_exports = HashSet::new();
                for (_, exports, _) in &packages {
                    for export in exports {
                        all_expected_exports.insert(export.clone());
                    }
                }

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                // Call get_all_exports on the first package
                if let Some((first_name, _, _)) = packages.first() {
                    let result = lib.get_all_exports(first_name).await;

                    // All exports from all packages should be in the result
                    // (since they're all connected via circular dependencies)
                    for export in &all_expected_exports {
                        prop_assert!(
                            result.contains(export),
                            "Result should contain export '{}' from the circular dependency graph",
                            export
                        );
                    }
                }

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7c: Self-dependencies (A depends on A) SHALL be handled without
        // infinite loops and return the package's exports.
        #[test]
        fn prop_self_dependency_terminates((pkg_name, exports) in self_dependency_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Create a package that depends on itself
                let info = PackageInfo::with_details(
                    pkg_name.clone(),
                    exports.clone(),
                    vec![pkg_name.clone()], // Self-dependency
                    Vec::new(),
                );
                lib.insert_package(info).await;

                // Call get_all_exports - should terminate without hanging
                let result = lib.get_all_exports(&pkg_name).await;

                // Should return the package's exports
                prop_assert_eq!(
                    result,
                    exports,
                    "Self-dependent package should return its own exports"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7d: For circular dependencies, calling get_all_exports from ANY
        // package in the cycle SHALL return the same set of exports.
        #[test]
        fn prop_circular_dependency_consistent_from_any_start(packages in circular_deps_strategy()) {
            // Only test if we have at least 2 packages
            prop_assume!(packages.len() >= 2);

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                // Get exports starting from first package
                let first_name = &packages[0].0;
                let result_from_first = lib.get_all_exports(first_name).await;

                // Get exports starting from second package
                let second_name = &packages[1].0;
                let result_from_second = lib.get_all_exports(second_name).await;

                // Both should return the same set of exports
                // (since all packages are connected via circular dependencies)
                prop_assert_eq!(
                    result_from_first,
                    result_from_second,
                    "get_all_exports should return same exports regardless of starting package in cycle"
                );

                Ok(())
            })?;
        }

        // Feature: package-function-awareness, Property 7: Circular Dependency Handling
        // **Validates: Requirement 4.5**
        //
        // Property 7e: Repeated calls to get_all_exports on circular dependencies
        // SHALL return identical results (idempotence with cycles).
        #[test]
        fn prop_circular_dependency_idempotent(packages in circular_deps_strategy()) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let lib = PackageLibrary::new_empty();

                // Insert all packages into the cache
                for (name, exports, depends) in &packages {
                    let info = PackageInfo::with_details(
                        name.clone(),
                        exports.clone(),
                        depends.clone(),
                        Vec::new(),
                    );
                    lib.insert_package(info).await;
                }

                if let Some((first_name, _, _)) = packages.first() {
                    // Call get_all_exports multiple times
                    let result1 = lib.get_all_exports(first_name).await;
                    let result2 = lib.get_all_exports(first_name).await;
                    let result3 = lib.get_all_exports(first_name).await;

                    // All results should be identical
                    prop_assert_eq!(
                        &result1,
                        &result2,
                        "First and second calls should return identical results"
                    );
                    prop_assert_eq!(
                        &result2,
                        &result3,
                        "Second and third calls should return identical results"
                    );
                }

                Ok(())
            })?;
        }
    }

    // ============================================================================
    // Additional unit tests for edge cases in circular dependency handling
    // ============================================================================

    #[tokio::test]
    async fn test_complex_cycle_a_b_c_a() {
        // Test complex cycle: A -> B -> C -> A
        let lib = PackageLibrary::new_empty();

        // Create package A (depends on B)
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        // Create package B (depends on C)
        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Create package C (depends on A - completing the cycle)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::with_details(
            "pkgC".to_string(),
            c_exports,
            vec!["pkgA".to_string()],
            Vec::new(),
        );
        lib.insert_package(c_info).await;

        // Should terminate and return all exports
        let all_exports = lib.get_all_exports("pkgA").await;

        assert!(all_exports.contains("a_func"), "Should have A's export");
        assert!(all_exports.contains("b_func"), "Should have B's export");
        assert!(all_exports.contains("c_func"), "Should have C's export");
        assert_eq!(all_exports.len(), 3);
    }

    #[tokio::test]
    async fn test_multiple_cycles_in_graph() {
        // Test graph with multiple cycles: A <-> B, C <-> D, A -> C
        let lib = PackageLibrary::new_empty();

        // Cycle 1: A <-> B
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgA".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        // Cycle 2: C <-> D
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::with_details(
            "pkgC".to_string(),
            c_exports,
            vec!["pkgD".to_string()],
            Vec::new(),
        );
        lib.insert_package(c_info).await;

        let mut d_exports = HashSet::new();
        d_exports.insert("d_func".to_string());
        let d_info = PackageInfo::with_details(
            "pkgD".to_string(),
            d_exports,
            vec!["pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(d_info).await;

        // Should terminate and return all exports from both cycles
        let all_exports = lib.get_all_exports("pkgA").await;

        assert!(all_exports.contains("a_func"));
        assert!(all_exports.contains("b_func"));
        assert!(all_exports.contains("c_func"));
        assert!(all_exports.contains("d_func"));
        assert_eq!(all_exports.len(), 4);
    }

    #[tokio::test]
    async fn test_cycle_with_external_dependency() {
        // Test cycle with an external non-cyclic dependency
        // A <-> B, both depend on C (no cycle)
        let lib = PackageLibrary::new_empty();

        // External package C (no cycle)
        let mut c_exports = HashSet::new();
        c_exports.insert("c_func".to_string());
        let c_info = PackageInfo::new("pkgC".to_string(), c_exports);
        lib.insert_package(c_info).await;

        // Cycle: A <-> B, both also depend on C
        let mut a_exports = HashSet::new();
        a_exports.insert("a_func".to_string());
        let a_info = PackageInfo::with_details(
            "pkgA".to_string(),
            a_exports,
            vec!["pkgB".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(a_info).await;

        let mut b_exports = HashSet::new();
        b_exports.insert("b_func".to_string());
        let b_info = PackageInfo::with_details(
            "pkgB".to_string(),
            b_exports,
            vec!["pkgA".to_string(), "pkgC".to_string()],
            Vec::new(),
        );
        lib.insert_package(b_info).await;

        let all_exports = lib.get_all_exports("pkgA").await;

        assert!(all_exports.contains("a_func"));
        assert!(all_exports.contains("b_func"));
        assert!(all_exports.contains("c_func"));
        assert_eq!(all_exports.len(), 3);
    }

    // ============================================================================
    // Tests for package_exists() - Task 10.3
    // ============================================================================

    #[test]
    fn test_package_exists_base_package() {
        // Test that base packages are always reported as existing
        // Validates: Requirement 15.1 (base packages should not trigger missing package diagnostic)
        let mut lib = PackageLibrary::new_empty();

        let mut base_packages = HashSet::new();
        base_packages.insert("base".to_string());
        base_packages.insert("stats".to_string());
        base_packages.insert("utils".to_string());
        lib.set_base_packages(base_packages);

        assert!(lib.package_exists("base"), "base should exist");
        assert!(lib.package_exists("stats"), "stats should exist");
        assert!(lib.package_exists("utils"), "utils should exist");
    }

    #[tokio::test]
    async fn test_package_exists_cached_package() {
        // Test that cached packages are reported as existing
        // Validates: Requirement 15.1 (cached packages should not trigger missing package diagnostic)
        let lib = PackageLibrary::new_empty();

        // Insert a package into cache
        let info = PackageInfo::new("dplyr".to_string(), HashSet::new());
        lib.insert_package(info).await;

        assert!(lib.package_exists("dplyr"), "cached package should exist");
    }

    #[test]
    fn test_package_exists_nonexistent_package() {
        // Test that non-existent packages are reported as not existing
        // Validates: Requirement 15.1 (non-installed packages should trigger diagnostic)
        let lib = PackageLibrary::new_empty();

        assert!(
            !lib.package_exists("__nonexistent_package_xyz__"),
            "non-existent package should not exist"
        );
    }

    #[tokio::test]
    async fn test_package_exists_with_r_subprocess() {
        // Test package_exists with actual R subprocess
        // Skip if R is not available
        let r_subprocess = match RSubprocess::new(None) {
            Some(s) => s,
            None => return,
        };

        let lib = PackageLibrary::new(Some(r_subprocess)).await;

        // Base packages should exist
        assert!(lib.package_exists("base"), "base should exist with R");
        assert!(lib.package_exists("stats"), "stats should exist with R");

        // Non-existent package should not exist
        assert!(
            !lib.package_exists("__nonexistent_package_xyz__"),
            "non-existent package should not exist"
        );
    }

    // ============================================================================
    // Tests for get_exports_for_completions() - Task 11.1
    // ============================================================================

    #[tokio::test]
    async fn test_get_exports_for_completions_empty_packages() {
        // Test that empty package list returns empty map
        // Validates: Requirement 9.1 (completions from loaded packages)
        let lib = PackageLibrary::new_empty();

        let result = lib.get_exports_for_completions(&[]);
        assert!(
            result.is_empty(),
            "Empty package list should return empty map"
        );
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_single_package() {
        // Test that exports from a single package are returned with package attribution
        // Validates: Requirements 9.1, 9.2
        let lib = PackageLibrary::new_empty();

        let mut exports = HashSet::new();
        exports.insert("mutate".to_string());
        exports.insert("filter".to_string());
        exports.insert("select".to_string());
        let info = PackageInfo::new("dplyr".to_string(), exports);
        lib.insert_package(info).await;

        let result = lib.get_exports_for_completions(&["dplyr".to_string()]);

        assert_eq!(result.len(), 3, "Should have 3 exports");
        assert_eq!(
            result.get("mutate"),
            Some(&vec!["dplyr".to_string()]),
            "mutate should be from dplyr"
        );
        assert_eq!(
            result.get("filter"),
            Some(&vec!["dplyr".to_string()]),
            "filter should be from dplyr"
        );
        assert_eq!(
            result.get("select"),
            Some(&vec!["dplyr".to_string()]),
            "select should be from dplyr"
        );
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_multiple_packages() {
        // Test that exports from multiple packages are returned with correct attribution
        // Validates: Requirements 9.1, 9.2
        let lib = PackageLibrary::new_empty();

        // Add dplyr
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        dplyr_exports.insert("filter".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        // Add ggplot2
        let mut ggplot2_exports = HashSet::new();
        ggplot2_exports.insert("ggplot".to_string());
        ggplot2_exports.insert("aes".to_string());
        let ggplot2_info = PackageInfo::new("ggplot2".to_string(), ggplot2_exports);
        lib.insert_package(ggplot2_info).await;

        let result = lib.get_exports_for_completions(&["dplyr".to_string(), "ggplot2".to_string()]);

        assert_eq!(result.len(), 4, "Should have 4 exports total");
        assert_eq!(result.get("mutate"), Some(&vec!["dplyr".to_string()]));
        assert_eq!(result.get("filter"), Some(&vec!["dplyr".to_string()]));
        assert_eq!(result.get("ggplot"), Some(&vec!["ggplot2".to_string()]));
        assert_eq!(result.get("aes"), Some(&vec!["ggplot2".to_string()]));
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_duplicate_exports_shows_all() {
        // Test that when multiple packages export the same symbol, all packages are shown
        // Validates: Requirement 9.3 (multiple packages export same symbol, show all)
        let lib = PackageLibrary::new_empty();

        // Add dplyr with filter
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("filter".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        // Add stats with filter (same name)
        let mut stats_exports = HashSet::new();
        stats_exports.insert("filter".to_string());
        let stats_info = PackageInfo::new("stats".to_string(), stats_exports);
        lib.insert_package(stats_info).await;

        // Both packages should be shown for filter, in load order
        let result = lib.get_exports_for_completions(&["dplyr".to_string(), "stats".to_string()]);
        assert_eq!(
            result.get("filter"),
            Some(&vec!["dplyr".to_string(), "stats".to_string()]),
            "Both dplyr and stats should be shown for filter, in load order"
        );

        // Reverse order: stats first, then dplyr
        let result2 = lib.get_exports_for_completions(&["stats".to_string(), "dplyr".to_string()]);
        assert_eq!(
            result2.get("filter"),
            Some(&vec!["stats".to_string(), "dplyr".to_string()]),
            "Both stats and dplyr should be shown for filter, in load order"
        );
    }

    #[tokio::test]
    async fn test_get_exports_for_completions_uncached_package() {
        // Test that uncached packages are skipped (no exports returned)
        // Validates: Requirement 9.1 (only cached packages contribute)
        let lib = PackageLibrary::new_empty();

        // Add dplyr to cache
        let mut dplyr_exports = HashSet::new();
        dplyr_exports.insert("mutate".to_string());
        let dplyr_info = PackageInfo::new("dplyr".to_string(), dplyr_exports);
        lib.insert_package(dplyr_info).await;

        // Request exports from dplyr (cached) and ggplot2 (not cached)
        let result = lib.get_exports_for_completions(&["dplyr".to_string(), "ggplot2".to_string()]);

        assert_eq!(
            result.len(),
            1,
            "Should only have exports from cached package"
        );
        assert_eq!(result.get("mutate"), Some(&vec!["dplyr".to_string()]));
        assert!(
            result.get("ggplot").is_none(),
            "ggplot should not be in results"
        );
    }

    #[test]
    fn test_get_exports_for_completions_synchronous() {
        // Test that the method works synchronously (no async required)
        // This is important for use in completion handlers
        let lib = PackageLibrary::new_empty();

        // The method should work even without async runtime
        let result = lib.get_exports_for_completions(&[]);
        assert!(result.is_empty());
    }
}
