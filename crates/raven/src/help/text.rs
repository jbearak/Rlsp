// help.rs - Minimal R help system for static LSP
//
// This module calls R as a subprocess to get help documentation.
// It's "static" in that it doesn't embed R, but can still access help.

use std::collections::HashMap;
use std::io::Read;
use std::process::Command;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use lru::LruCache;

/// Maximum number of entries in the help cache. When this limit is reached,
/// the least-recently-used entries are evicted to make room for new ones.
const HELP_CACHE_MAX_ENTRIES: usize = 512;

/// Duration after which negative cache entries (no help found) expire.
/// Positive entries never expire since help text doesn't change during a session.
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(300);

/// A cached help lookup result, with a timestamp for TTL-based expiry of
/// negative entries.
#[derive(Clone)]
struct CachedHelp {
    content: Option<String>,
    cached_at: Instant,
}

/// Type alias to reduce complexity of the nested generics in `HelpCache.arguments`.
type ArgumentsCache = LruCache<String, Option<HashMap<String, String>>>;

/// Bounded cache for help content. Uses LRU eviction to prevent unbounded
/// memory growth in long-running LSP sessions.
///
/// Keys use composite format: `"package\0topic"` for package-qualified lookups,
/// plain `"topic"` for unqualified lookups. The null-byte separator prevents
/// collisions since R identifiers cannot contain null bytes.
///
/// Negative entries (no help found) expire after a configurable TTL so that
/// transient R failures don't permanently suppress help lookups.
///
/// Uses `peek()` for reads (no LRU promotion, works under read lock) and
/// `push()` for writes (promotes/evicts under write lock).
///
/// Also caches structured argument maps (parameter name → description) parsed
/// from the "Arguments:" section of rendered help text. This avoids re-parsing
/// help text on every `completionItem/resolve` request for parameter documentation.
#[derive(Clone)]
pub struct HelpCache {
    inner: Arc<RwLock<LruCache<String, CachedHelp>>>,
    /// Cache for structured argument maps: topic+package → HashMap<param_name, description>.
    /// Uses the same composite key format as `inner`. `None` values represent
    /// negative cache entries (no arguments found or no help available).
    arguments: Arc<RwLock<ArgumentsCache>>,
    negative_ttl: Duration,
}

/// Builds a cache key from a topic and optional package.
///
/// Uses `\0` as separator since R identifiers cannot contain null bytes,
/// preventing collisions between e.g. unqualified topic `"a::b"` and
/// package `"a"` + topic `"b"`.
fn cache_key(topic: &str, package: Option<&str>) -> String {
    match package {
        Some(pkg) => format!("{pkg}\0{topic}"),
        None => topic.to_string(),
    }
}

impl HelpCache {
    pub fn new() -> Self {
        Self::with_config(HELP_CACHE_MAX_ENTRIES, NEGATIVE_CACHE_TTL)
    }

    /// Returns cached help content, or `None` on cache miss.
    ///
    /// Returns `Some(None)` for a negative cache hit (topic was looked up but
    /// had no help). Expired negative entries are evicted and treated as cache
    /// misses. Returns `Some(Some(text))` for a positive cache hit.
    pub fn get(&self, topic: &str, package: Option<&str>) -> Option<Option<String>> {
        let key = cache_key(topic, package);

        // Check under read lock (concurrent reads allowed)
        {
            let guard = self.inner.read().ok()?;
            let entry = guard.peek(&key)?;
            if entry.content.is_some() {
                return Some(entry.content.clone());
            }
            // Negative entry — return if still fresh
            if entry.cached_at.elapsed() <= self.negative_ttl {
                return Some(None);
            }
            // Expired — drop read lock, fall through to evict
        }

        // Evict the expired entry under a write lock so it doesn't waste a slot
        if let Ok(mut guard) = self.inner.write() {
            // Re-check: another thread may have refreshed or removed it
            let still_expired = guard
                .peek(&key)
                .map(|e| e.content.is_none() && e.cached_at.elapsed() > self.negative_ttl)
                .unwrap_or(false);
            if still_expired {
                guard.pop(&key);
            }
        }

        None
    }

    pub fn insert(&self, topic: &str, package: Option<&str>, content: Option<String>) {
        let key = cache_key(topic, package);
        let entry = CachedHelp {
            content,
            cached_at: Instant::now(),
        };
        if let Ok(mut guard) = self.inner.write() {
            guard.push(key, entry);
        }
    }

    /// Looks up help text, using the cache to avoid redundant R subprocess calls.
    ///
    /// On cache hit, returns the cached content. On cache miss, calls `get_help()`
    /// to spawn an R subprocess, caches the result (including negative), and returns it.
    ///
    /// **Note:** This method blocks on R subprocess I/O. Callers on an async
    /// runtime should use `tokio::task::spawn_blocking`.
    pub fn get_or_fetch(
        &self,
        topic: &str,
        package: Option<&str>,
        r_path: &std::path::Path,
    ) -> Option<String> {
        if let Some(cached) = self.get(topic, package) {
            return cached;
        }
        let result = get_help(topic, package, r_path);
        self.insert(topic, package, result.clone());
        result
    }

    /// Returns structured argument documentation for a function, using the cache
    /// to avoid re-parsing help text on every resolve request.
    ///
    /// On cache hit, returns the cached argument map. On cache miss, fetches help
    /// text (via `get_or_fetch`, which itself caches the raw help text), parses
    /// the "Arguments:" section, caches the result, and returns it.
    ///
    /// Returns `Some(HashMap)` when arguments are found, `None` when no help text
    /// is available or the help text has no "Arguments:" section.
    ///
    /// **Note:** This method may block on R subprocess I/O (on help text cache miss).
    /// Callers on an async runtime should use `tokio::task::spawn_blocking`.
    pub fn get_arguments(
        &self,
        topic: &str,
        package: Option<&str>,
        r_path: &std::path::Path,
    ) -> Option<HashMap<String, String>> {
        let key = cache_key(topic, package);

        // Check argument cache under read lock (concurrent reads allowed)
        {
            if let Ok(guard) = self.arguments.read()
                && let Some(cached) = guard.peek(&key)
            {
                return cached.clone();
            }
        }

        // Cache miss — fetch help text and parse arguments
        // Transient R failure — don't cache so retries can succeed later.
        let help_text = self.get_or_fetch(topic, package, r_path)?;
        let map = extract_arguments_from_help(&help_text);
        let args = if map.is_empty() { None } else { Some(map) };

        // Cache the parsed result (including None for "no Arguments section" cases)
        if let Ok(mut guard) = self.arguments.write() {
            guard.push(key, args.clone());
        }

        args
    }

    /// Clear all cached help entries (positive, negative, and arguments).
    ///
    /// Called when libpath changes invalidate the help corpus (package install,
    /// uninstall, or upgrade).
    pub fn drain(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.clear();
        }
        if let Ok(mut guard) = self.arguments.write() {
            guard.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.read().map(|c| c.len()).unwrap_or(0)
    }

    fn with_config(cap: usize, negative_ttl: Duration) -> Self {
        let cap = crate::cross_file::cache::non_zero_or(cap, HELP_CACHE_MAX_ENTRIES);
        Self {
            inner: Arc::new(RwLock::new(LruCache::new(cap))),
            arguments: Arc::new(RwLock::new(LruCache::new(cap))),
            negative_ttl,
        }
    }

    #[cfg(test)]
    fn with_max_entries(cap: usize) -> Self {
        Self::with_config(cap, NEGATIVE_CACHE_TTL)
    }

    #[cfg(test)]
    fn with_max_entries_and_ttl(cap: usize, negative_ttl: Duration) -> Self {
        Self::with_config(cap, negative_ttl)
    }
}

impl Default for HelpCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Timeout for R help subprocess calls. Prevents hung R processes from
/// blocking threads indefinitely.
const HELP_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns the effective timeout, overridable by `RAVEN_HELP_TIMEOUT_MS` for tests.
fn timeout_from_env() -> Duration {
    match std::env::var("RAVEN_HELP_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(ms) => Duration::from_millis(ms),
        None => HELP_TIMEOUT,
    }
}

/// Fetches help text for an R topic by invoking R and returns the rendered help if available.
///
/// Returns `Some(String)` containing the help text when R executes successfully, the output is not empty,
/// and the output does not contain the phrase "No documentation". Returns `None` otherwise.
///
/// The subprocess is killed after `HELP_TIMEOUT` to prevent hung R processes from
/// blocking threads indefinitely.
///
/// # Examples
///
/// ```no_run
/// // Attempt to get help for `mean` from the base package
/// # let r_path = std::path::Path::new("R");
/// if let Some(text) = raven::help::get_help("mean", Some("base"), r_path) {
///     assert!(text.contains("Usage:"));
/// }
/// ```
pub fn get_help(topic: &str, package: Option<&str>, r_path: &std::path::Path) -> Option<String> {
    let r_code = r#"
args <- commandArgs(trailingOnly = TRUE)
topic <- args[1]
pkg <- if (length(args) >= 2 && nzchar(args[2])) args[2] else NULL
txt <- capture.output(
  tools::Rd2txt(
    utils:::.getHelpFile(help(topic, package = (pkg))),
    options = list(underline_titles = FALSE)
  )
)
cat(paste(txt, collapse = "\n"))
"#;
    get_help_inner(topic, package, r_path, r_code, None)
}

/// Inner implementation that accepts the R script as a parameter.
///
/// This allows tests to inject custom R code (e.g., `Sys.sleep(60)` for timeout
/// testing) without duplicating the full implementation. The public `get_help`
/// always passes the built-in R snippet.
///
/// The optional `pid_capture` parameter, when provided, receives the spawned child's
/// PID immediately after a successful `spawn()`. This is used only in tests to verify
/// that the watchdog reaps the child process after a timeout.
fn get_help_inner(
    topic: &str,
    package: Option<&str>,
    r_path: &std::path::Path,
    r_code: &str,
    pid_capture: Option<&AtomicU32>,
) -> Option<String> {
    log::trace!("get_help: topic={}, package={:?}", topic, package);

    if !super::validate::is_valid_help_topic(topic) {
        log::trace!("get_help: topic {:?} failed validation", topic);
        return None;
    }
    if let Some(p) = package
        && !crate::r_subprocess::is_valid_package_name(p)
    {
        log::trace!("get_help: package {:?} failed validation", p);
        return None;
    }

    let timeout = timeout_from_env();

    let mut cmd = Command::new(r_path);
    cmd.args([
        "--slave",
        "--no-save",
        "--no-restore",
        "-e",
        r_code,
        "--args",
        topic,
    ]);
    if let Some(pkg) = package {
        cmd.arg(pkg);
    }

    // Spawn as child process so we can enforce a timeout.
    // Pipe both stdout (for help text) and stderr (to prevent it polluting
    // the LSP server's stderr stream).
    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log::trace!("get_help: subprocess spawn error: {}", e);
            return None;
        }
    };

    let child_pid = child.id();
    // Store the pid before heavy work so tests can observe it even if we return early.
    if let Some(cap) = pid_capture {
        cap.store(child_pid, std::sync::atomic::Ordering::SeqCst);
    }

    // Take pipe handles before spawning threads. Reading pipes in separate
    // threads prevents deadlock when pipe buffers fill before the child exits.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_thread = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe
            && let Err(e) = pipe.read_to_end(&mut buf)
        {
            log::trace!("get_help: failed to read stdout pipe: {}", e);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe
            && let Err(e) = pipe.read_to_end(&mut buf)
        {
            log::trace!("get_help: failed to read stderr pipe: {}", e);
        }
        buf
    });

    // Watchdog: a sleeper thread sends a one-shot timeout signal. The main
    // thread polls `try_wait()` and the signal channel together. We kill via
    // `child.kill()` only after a `try_wait()` that returned `Ok(None)` —
    // meaning we have NOT reaped the PID yet — so the kill cannot land on a
    // recycled PID. This is the structural cure for the old "atomic flag plus
    // bare-PID SIGKILL from a detached thread" race, where a watchdog firing
    // between `wait()` reaping the child and the atomic store could SIGKILL
    // an unrelated process that had grabbed the freed PID.
    let (timeout_tx, timeout_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        let _ = timeout_tx.send(());
    });

    // Poll just slow enough that idle CPU cost is negligible (~20 wakeups/s)
    // but small enough that a fast subprocess returns near-instantly.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    let wait_result: std::io::Result<std::process::ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if timeout_rx.try_recv().is_ok() {
                    log::trace!(
                        "get_help: timeout after {:?}, killing pid {}",
                        timeout,
                        child_pid
                    );
                    // Safe: try_wait returned None, so the child has not been
                    // reaped and `child_pid` still names this child.
                    let _ = child.kill();
                    break child.wait();
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => break Err(e),
        }
    };

    let stdout_bytes = stdout_thread.join().unwrap_or_default();
    let stderr_bytes = stderr_thread.join().unwrap_or_default();

    match wait_result {
        Ok(status) => {
            log::trace!(
                "get_help: exit_status={}, stdout_len={}, stderr_len={}",
                status,
                stdout_bytes.len(),
                stderr_bytes.len()
            );
            if !stderr_bytes.is_empty() {
                log::trace!(
                    "get_help: stderr: {}",
                    String::from_utf8_lossy(&stderr_bytes)
                );
            }
            if status.success() {
                let text = String::from_utf8_lossy(&stdout_bytes).to_string();
                if !text.trim().is_empty() && !text.contains("No documentation") {
                    return Some(text);
                }
                log::trace!("get_help: empty or no documentation");
            }
            None
        }
        Err(e) => {
            log::trace!("get_help: wait error: {}", e);
            None
        }
    }
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
/// use raven::help::extract_signature_from_help;
///
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
/// ```no_run
/// use raven::help::get_function_signature;
///
/// # let r_path = std::path::Path::new("R");
/// let sig = get_function_signature("mean", "base", r_path);
/// if let Some(s) = sig {
///     println!("{}", s);
/// }
/// ```
pub fn get_function_signature(
    topic: &str,
    package: &str,
    r_path: &std::path::Path,
) -> Option<String> {
    let help_text = get_help(topic, Some(package), r_path)?;
    extract_signature_from_help(&help_text)
}

/// Converts R help text's Unicode curly quotes to markdown inline code.
///
/// R help uses U+2018 (') and U+2019 (') to wrap code references.
/// This converts `'code'` patterns to `` `code` `` for markdown rendering.
fn r_quotes_to_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{2018}' {
            // Left curly quote — collect until matching right curly quote
            let mut code = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == '\u{2019}' {
                    found_close = true;
                    break;
                }
                code.push(inner);
            }
            if found_close && !code.is_empty() {
                result.push('`');
                result.push_str(&code);
                result.push('`');
            } else {
                // Unmatched quote — emit as-is
                result.push('\u{2018}');
                result.push_str(&code);
            }
        } else if ch == '\u{2019}' {
            // Stray right curly quote — pass through
            result.push(ch);
        } else {
            result.push(ch);
        }
    }

    result
}

/// Extracts the title and description from R help text for use in completion documentation.
///
/// Returns a formatted string with the title (first line) and the "Description:" section content.
/// Stops at the next section header (e.g., "Usage:", "Arguments:").
///
/// # Examples
///
/// ```
/// use raven::help::extract_description_from_help;
///
/// let help = r#"Arithmetic Mean
///
/// Description:
///
///      Generic function for the (trimmed) arithmetic mean.
///
/// Usage:
///      mean(x, ...)
/// "#;
///
/// let desc = extract_description_from_help(help);
/// assert!(desc.is_some());
/// let text = desc.unwrap();
/// assert!(text.contains("Arithmetic Mean"));
/// assert!(text.contains("arithmetic mean"));
/// ```
pub fn extract_description_from_help(help_text: &str) -> Option<String> {
    let lines: Vec<&str> = help_text.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // First non-empty line is the title
    let title = lines.iter().find(|l| !l.trim().is_empty())?.trim();

    // Find the "Description:" section
    let desc_idx = lines
        .iter()
        .position(|line| line.trim() == "Description:")?;

    // Collect lines after "Description:" until we hit another section header
    let mut desc_lines: Vec<&str> = Vec::new();
    for line in lines.iter().skip(desc_idx + 1) {
        let trimmed = line.trim();

        // Stop at the next section header (non-empty, ends with ':', no parens)
        if !trimmed.is_empty() && trimmed.ends_with(':') && !trimmed.contains('(') {
            break;
        }

        desc_lines.push(trimmed);
    }

    // Trim leading/trailing empty lines
    let start = desc_lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(desc_lines.len());
    let end = desc_lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(start, |i| i + 1);
    let desc_lines = &desc_lines[start..end];

    if desc_lines.is_empty() {
        return Some(r_quotes_to_markdown(title));
    }

    let description = desc_lines.join(" ");
    // Collapse multiple spaces from line joining
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");

    Some(r_quotes_to_markdown(&format!("{title}\n\n{description}")))
}

/// Extracts structured argument documentation from the "Arguments:" section
/// of rendered R help text (produced by `tools::Rd2txt()`).
///
/// Returns a `HashMap` mapping parameter names to their descriptions. Multi-line
/// descriptions are joined with spaces and whitespace is normalized. R's Unicode
/// curly quotes are converted to markdown backticks.
///
/// The parser handles the standard `Rd2txt` output format where each argument
/// entry looks like:
///
/// ```text
///        x: An R object. Currently there are methods for
///           numeric/logical vectors.
///
///     trim: the fraction (0 to 0.5) of observations to be
///           trimmed from each end.
/// ```
///
/// The argument name is right-aligned before the colon, and continuation lines
/// are indented to align with the description start column.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
/// use raven::help::extract_arguments_from_help;
///
/// let help = r#"Arithmetic Mean
///
/// Description:
///
///      Generic function for the (trimmed) arithmetic mean.
///
/// Usage:
///
///      mean(x, ...)
///
/// Arguments:
///
///        x: An R object.
///
///     trim: the fraction (0 to 0.5) of observations to be trimmed.
///
///    na.rm: a logical value indicating whether NA values should be
///           stripped.
///
///      ...: further arguments passed to or from other methods.
///
/// Value:
///
///      The mean.
/// "#;
///
/// let args = extract_arguments_from_help(help);
/// assert_eq!(args.get("x").unwrap(), "An R object.");
/// assert!(args.get("trim").unwrap().contains("fraction"));
/// assert!(args.get("na.rm").unwrap().contains("logical"));
/// assert!(args.get("...").unwrap().contains("further arguments"));
/// ```
pub fn extract_arguments_from_help(help_text: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let lines: Vec<&str> = help_text.lines().collect();

    // Find the "Arguments:" section
    let args_idx = match lines.iter().position(|line| line.trim() == "Arguments:") {
        Some(idx) => idx,
        None => return result,
    };

    // Track the current argument being parsed
    let mut current_name: Option<String> = None;
    let mut current_desc_parts: Vec<String> = Vec::new();
    // The column where the description text starts (after "name: "),
    // used to detect continuation lines vs new argument entries.
    let mut desc_start_col: Option<usize> = None;

    for line in lines.iter().skip(args_idx + 1) {
        let trimmed = line.trim();

        // Stop at the next section header (non-empty, ends with ':', starts at
        // column 0). Argument entries have leading whitespace, so checking
        // !line.starts_with(' ') distinguishes headers from argument lines.
        // This correctly handles headers like "Author(s):" that contain parens.
        if !trimmed.is_empty() && trimmed.ends_with(':') && !line.starts_with(' ') {
            break;
        }

        // Empty line — could be separator between arguments or within section
        if trimmed.is_empty() {
            continue;
        }

        // Try to match an argument entry: "   name: description"
        // The pattern is: leading whitespace, then a name (which may contain
        // dots, letters, digits, or be "..."), then ": ", then description.
        if let Some(colon_pos) = line.find(": ") {
            let before_colon = &line[..colon_pos];
            let name_trimmed = before_colon.trim();

            // Validate this looks like an R parameter name:
            // - Must not be empty
            // - Must start with a letter, dot, or be "..."
            // - The part before the colon must be indented (leading whitespace)
            if !name_trimmed.is_empty()
                && before_colon.starts_with(' ')
                && is_r_param_name(name_trimmed)
            {
                // Save previous argument if any
                if let Some(name) = current_name.take() {
                    let desc = finalize_description(&current_desc_parts);
                    if !desc.is_empty() {
                        result.insert(name, desc);
                    }
                    current_desc_parts.clear();
                }

                // Start new argument
                current_name = Some(name_trimmed.to_string());
                let desc_text = &line[colon_pos + 2..];
                current_desc_parts.push(desc_text.trim().to_string());
                // Record where the description starts for continuation detection
                desc_start_col = Some(colon_pos + 2);
                continue;
            }
        }

        // Check for continuation line: must be indented at least as far as
        // the description start column of the current argument
        if current_name.is_some() {
            let line_indent = line.len() - line.trim_start().len();
            // Continuation lines are indented to align with description start,
            // or at least significantly indented (>= 6 spaces is a safe heuristic
            // for Rd2txt output which typically indents 5+ spaces)
            if let Some(start_col) = desc_start_col {
                if line_indent >= start_col.saturating_sub(2) {
                    current_desc_parts.push(trimmed.to_string());
                    continue;
                }
            } else if line_indent >= 6 {
                current_desc_parts.push(trimmed.to_string());
                continue;
            }
        }
    }

    // Save the last argument
    if let Some(name) = current_name {
        let desc = finalize_description(&current_desc_parts);
        if !desc.is_empty() {
            result.insert(name, desc);
        }
    }

    result
}

/// Checks if a string looks like a valid R parameter name.
///
/// Valid R parameter names consist of letters, digits, dots, and underscores.
/// The special `...` (dots) parameter is also valid.
fn is_r_param_name(s: &str) -> bool {
    if s == "..." {
        return true;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // R identifiers must start with a letter or dot (dot not followed by digit)
    if !(first.is_alphabetic() || first == '.') {
        return false;
    }
    if first == '.'
        && let Some(&next) = s.as_bytes().get(1)
        && next.is_ascii_digit()
    {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '.' || c == '_')
}

/// Joins description parts into a single string, normalizing whitespace
/// and converting R's Unicode curly quotes to markdown backticks.
fn finalize_description(parts: &[String]) -> String {
    let joined = parts.join(" ");
    let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    r_quotes_to_markdown(&normalized)
}

/// Test-only entry point that injects custom R code instead of the built-in snippet.
///
/// Used by timeout tests to inject hanging R code (e.g., `Sys.sleep(60)`) without
/// touching the public API or duplicating the full implementation.
#[cfg(test)]
pub(crate) fn get_help_with_code(
    topic: &str,
    package: Option<&str>,
    r_path: &std::path::Path,
    r_code: &str,
) -> Option<String> {
    get_help_inner(topic, package, r_path, r_code, None)
}

/// Test-only entry point that injects custom R code and captures the spawned PID.
///
/// Writes the child's PID into `pid_capture` immediately after a successful spawn,
/// allowing the caller to probe whether the watchdog has reaped the process.
#[cfg(test)]
pub(crate) fn get_help_with_code_capturing_pid(
    topic: &str,
    package: Option<&str>,
    r_path: &std::path::Path,
    r_code: &str,
    pid_capture: &AtomicU32,
) -> Option<String> {
    get_help_inner(topic, package, r_path, r_code, Some(pid_capture))
}

/// RAII guard that sets an environment variable on creation and restores the
/// prior value (or removes the var if it was unset) on drop.
///
/// Keeps env-var mutations local to a single test even in the presence of
/// panics. Storing the previous value matters when a parent process or an
/// outer test set the variable: unconditionally removing on drop would
/// clobber that state.
#[cfg(test)]
struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: single-threaded test context; we restore in Drop.
        unsafe { std::env::set_var(key, val) };
        EnvGuard { key, previous }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: mirrors the set_var above.
        unsafe {
            match self.previous.take() {
                Some(prev) => std::env::set_var(self.key, prev),
                None => std::env::remove_var(self.key),
            }
        }
    }
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

    #[test]
    fn test_help_cache_bounded() {
        let cache = HelpCache::with_max_entries(3);

        cache.insert("a", None, Some("help_a".to_string()));
        cache.insert("b", None, Some("help_b".to_string()));
        cache.insert("c", None, Some("help_c".to_string()));
        assert_eq!(cache.len(), 3);

        // Inserting a 4th entry should evict the LRU ("a")
        cache.insert("d", None, Some("help_d".to_string()));
        assert_eq!(cache.len(), 3);
        assert!(cache.get("a", None).is_none());
        assert!(cache.get("d", None).is_some());
    }

    #[test]
    fn test_help_cache_update_existing() {
        let cache = HelpCache::with_max_entries(3);

        cache.insert("a", None, None);
        assert_eq!(cache.get("a", None), Some(None));

        // Updating same key should not grow the cache
        cache.insert("a", None, Some("updated".to_string()));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("a", None), Some(Some("updated".to_string())));
    }

    #[test]
    fn test_help_cache_composite_key() {
        let cache = HelpCache::with_max_entries(10);

        // Same topic, different packages should be separate entries
        cache.insert("filter", Some("dplyr"), Some("dplyr filter".to_string()));
        cache.insert("filter", Some("stats"), Some("stats filter".to_string()));
        cache.insert("filter", None, Some("unqualified filter".to_string()));

        assert_eq!(cache.len(), 3);
        assert_eq!(
            cache.get("filter", Some("dplyr")),
            Some(Some("dplyr filter".to_string()))
        );
        assert_eq!(
            cache.get("filter", Some("stats")),
            Some(Some("stats filter".to_string()))
        );
        assert_eq!(
            cache.get("filter", None),
            Some(Some("unqualified filter".to_string()))
        );
    }

    #[test]
    fn test_help_cache_negative_caching() {
        let cache = HelpCache::with_max_entries(10);

        // Insert a negative result (topic exists in cache but has no help)
        cache.insert("nonexistent_func", Some("somepkg"), None);

        // get() should return Some(None) — "we looked it up and there's nothing"
        // This is distinct from a cache miss which returns None
        let result = cache.get("nonexistent_func", Some("somepkg"));
        assert_eq!(
            result,
            Some(None),
            "Negative cache entry should return Some(None), not a cache miss"
        );

        // A different package for the same topic should be a cache miss
        let result = cache.get("nonexistent_func", Some("otherpkg"));
        assert!(
            result.is_none(),
            "Different package should be a cache miss, not a negative hit"
        );
    }

    #[test]
    fn test_help_cache_clone_shares_state() {
        // This property is critical: backend.rs clones the cache before
        // spawn_blocking, so writes in the blocking thread must be visible
        // to the original cache (and vice versa).
        let cache1 = HelpCache::with_max_entries(10);
        let cache2 = cache1.clone();

        // Write through clone, read through original
        cache2.insert("mean", Some("base"), Some("help text".to_string()));
        assert_eq!(
            cache1.get("mean", Some("base")),
            Some(Some("help text".to_string())),
            "Clone and original must share underlying storage"
        );

        // Write through original, read through clone
        cache1.insert("filter", Some("dplyr"), Some("dplyr help".to_string()));
        assert_eq!(
            cache2.get("filter", Some("dplyr")),
            Some(Some("dplyr help".to_string())),
            "Writes through original must be visible through clone"
        );
    }

    #[test]
    fn test_help_cache_negative_ttl_expiry() {
        // Negative entries should expire after the TTL so that transient R
        // failures (e.g., timeout, R busy) don't permanently suppress lookups.
        let cache = HelpCache::with_max_entries_and_ttl(10, Duration::from_millis(50));

        cache.insert("hung_func", Some("slow_pkg"), None);
        assert_eq!(cache.len(), 1);

        // Immediately after insertion, negative entry should be a cache hit
        assert_eq!(
            cache.get("hung_func", Some("slow_pkg")),
            Some(None),
            "Fresh negative entry should be a cache hit"
        );

        // Wait for TTL to expire
        std::thread::sleep(Duration::from_millis(80));

        // After TTL, negative entry should be treated as a cache miss
        assert!(
            cache.get("hung_func", Some("slow_pkg")).is_none(),
            "Expired negative entry should be treated as cache miss, allowing retry"
        );

        // The expired entry should have been evicted from the LRU map, not just
        // hidden — it should not waste a cache slot
        assert_eq!(
            cache.len(),
            0,
            "Expired negative entry should be evicted from the cache, not left as a stale slot"
        );
    }

    #[test]
    fn test_help_cache_positive_entries_never_expire() {
        // Positive entries (actual help text) should never expire — help text
        // doesn't change during an LSP session.
        let cache = HelpCache::with_max_entries_and_ttl(10, Duration::from_millis(50));

        cache.insert("mean", Some("base"), Some("help text".to_string()));

        // Wait well past the negative TTL
        std::thread::sleep(Duration::from_millis(80));

        // Positive entry should still be a cache hit
        assert_eq!(
            cache.get("mean", Some("base")),
            Some(Some("help text".to_string())),
            "Positive cache entries must never expire"
        );
    }

    #[test]
    fn test_help_cache_get_or_fetch_uses_cache() {
        // get_or_fetch should return cached content without calling get_help().
        // We verify this by pre-populating the cache with synthetic help text
        // that R would never produce, then checking get_or_fetch returns it.
        // r_path is required by the signature but never invoked on a cache hit.
        let cache = HelpCache::with_max_entries(10);
        let synthetic = "SYNTHETIC_HELP_TEXT_NOT_FROM_R";
        cache.insert("fake_topic", Some("fake_pkg"), Some(synthetic.to_string()));

        let dummy_r = std::path::Path::new("R");
        let result = cache.get_or_fetch("fake_topic", Some("fake_pkg"), dummy_r);
        assert_eq!(
            result,
            Some(synthetic.to_string()),
            "get_or_fetch should return cached content without spawning R"
        );
    }

    #[test]
    fn test_help_cache_get_or_fetch_caches_negative() {
        // When get_help returns None (nonexistent package), get_or_fetch should
        // cache the negative result so subsequent calls don't spawn R again.
        let cache = HelpCache::with_max_entries(10);

        let r = match crate::r_subprocess::RSubprocess::new(None) {
            Some(s) => s.r_path().clone(),
            None => {
                eprintln!("skip: no R");
                return;
            }
        };
        let _ = cache.get_or_fetch("no_such_func", Some("nonexistent_pkg_xyzzy"), &r);

        let cached = cache.get("no_such_func", Some("nonexistent_pkg_xyzzy"));
        assert_eq!(
            cached,
            Some(None),
            "get_or_fetch should cache negative results from R subprocess"
        );
    }

    #[test]
    fn test_help_cache_negative_ttl_refresh_end_to_end() {
        // End-to-end test: a negative entry expires after TTL, and a subsequent
        // get_or_fetch() re-populates the cache (rather than returning stale data
        // or permanently treating the topic as missing).
        let r = match crate::r_subprocess::RSubprocess::new(None) {
            Some(s) => s.r_path().clone(),
            None => {
                eprintln!("skip: no R");
                return;
            }
        };
        let cache = HelpCache::with_max_entries_and_ttl(10, Duration::from_millis(50));

        // Simulate a failed lookup by inserting a negative entry directly
        cache.insert("some_func", Some("nonexistent_pkg_xyzzy"), None);
        assert_eq!(
            cache.get("some_func", Some("nonexistent_pkg_xyzzy")),
            Some(None),
            "Fresh negative entry should be a cache hit"
        );

        // Wait for the TTL to expire
        std::thread::sleep(Duration::from_millis(80));

        // get() should now treat the expired entry as a cache miss
        assert!(
            cache
                .get("some_func", Some("nonexistent_pkg_xyzzy"))
                .is_none(),
            "Expired negative entry should be a cache miss"
        );

        // get_or_fetch() should re-fetch from R (which will fail again for this
        // fake package) and re-populate the cache with a fresh negative entry
        let result = cache.get_or_fetch("some_func", Some("nonexistent_pkg_xyzzy"), &r);
        assert!(
            result.is_none(),
            "R should still not find this fake package"
        );

        // The cache should now have a fresh negative entry (not expired)
        assert_eq!(
            cache.get("some_func", Some("nonexistent_pkg_xyzzy")),
            Some(None),
            "get_or_fetch should have re-cached a fresh negative entry after TTL expiry"
        );
    }

    #[test]
    fn test_extract_description_basic() {
        let help_text = r#"Arithmetic Mean

Description:

     Generic function for the (trimmed) arithmetic mean.

Usage:

     mean(x, ...)
"#;

        let desc = extract_description_from_help(help_text);
        assert!(desc.is_some());
        let text = desc.unwrap();
        assert!(text.starts_with("Arithmetic Mean"));
        assert!(text.contains("arithmetic mean"));
    }

    #[test]
    fn test_extract_description_multiline() {
        // Use Unicode curly quotes (U+2018, U+2019) matching actual R help output
        let help_text = "Read R Code from a File, a Connection or Expressions\n\
            \n\
            Description:\n\
            \n\
            \x20\x20\x20\x20\x20\u{2018}source\u{2019} causes R to accept its input from the named file or URL\n\
            \x20\x20\x20\x20\x20or connection or expressions directly.  Input is read and \u{2018}parse\u{2019}d\n\
            \x20\x20\x20\x20\x20from that file until the end of the file is reached.\n\
            \n\
            Usage:\n\
            \n\
            \x20\x20\x20\x20\x20source(file, local = FALSE)\n";

        let desc = extract_description_from_help(help_text).unwrap();
        assert!(desc.starts_with("Read R Code from a File"));
        // Unicode curly quotes should be converted to markdown backticks
        assert!(
            desc.contains("`source`"),
            "should convert curly quotes to backticks: {desc}"
        );
        assert!(
            desc.contains("`parse`"),
            "should convert curly quotes to backticks: {desc}"
        );
        assert!(desc.contains("named file or URL"));
        // Should not include Usage section
        assert!(!desc.contains("local = FALSE"));
    }

    #[test]
    fn test_extract_description_no_description_section() {
        let help_text = "Some Title\n\nUsage:\n    foo(x)\n";
        assert!(extract_description_from_help(help_text).is_none());
    }

    #[test]
    fn test_extract_description_empty() {
        assert!(extract_description_from_help("").is_none());
    }

    #[test]
    fn test_extract_description_title_only() {
        let help_text = "My Function\n\nDescription:\n\n";
        let desc = extract_description_from_help(help_text).unwrap();
        assert_eq!(desc, "My Function");
    }

    #[test]
    fn test_r_quotes_to_markdown_basic() {
        let input = "\u{2018}source\u{2019} causes R to do things";
        let result = r_quotes_to_markdown(input);
        assert_eq!(result, "`source` causes R to do things");
    }

    #[test]
    fn test_r_quotes_to_markdown_multiple() {
        let input = "\u{2018}foo\u{2019} and \u{2018}bar\u{2019}";
        let result = r_quotes_to_markdown(input);
        assert_eq!(result, "`foo` and `bar`");
    }

    #[test]
    fn test_r_quotes_to_markdown_with_parens() {
        let input = "\u{2018}withAutoprint(exprs)\u{2019} is a wrapper";
        let result = r_quotes_to_markdown(input);
        assert_eq!(result, "`withAutoprint(exprs)` is a wrapper");
    }

    #[test]
    fn test_r_quotes_to_markdown_no_quotes() {
        let input = "No special quotes here";
        let result = r_quotes_to_markdown(input);
        assert_eq!(result, "No special quotes here");
    }

    #[test]
    fn test_r_quotes_to_markdown_unmatched() {
        // Unmatched left quote should be preserved
        let input = "text \u{2018}unmatched";
        let result = r_quotes_to_markdown(input);
        assert_eq!(result, "text \u{2018}unmatched");
    }

    // --- Tests for extract_arguments_from_help ---

    #[test]
    fn test_extract_arguments_basic() {
        let help_text = r#"Arithmetic Mean

Description:

     Generic function for the (trimmed) arithmetic mean.

Usage:

     mean(x, ...)

Arguments:

       x: An R object.

     ...: further arguments passed to or from other methods.

Value:

     The mean.
"#;

        let args = extract_arguments_from_help(help_text);
        assert_eq!(args.len(), 2);
        assert_eq!(args.get("x").unwrap(), "An R object.");
        assert!(args.get("...").unwrap().contains("further arguments"));
    }

    #[test]
    fn test_extract_arguments_multiline_description() {
        let help_text = r#"Arithmetic Mean

Description:

     Generic function for the (trimmed) arithmetic mean.

Usage:

     mean(x, trim = 0, na.rm = FALSE, ...)

Arguments:

       x: An R object.  Currently there are methods for numeric/logical
          vectors and date, date-time and time interval objects.

    trim: the fraction (0 to 0.5) of observations to be trimmed from
          each end of 'x' before the mean is computed.  Values of trim
          outside that range are taken as the nearest endpoint.

   na.rm: a logical evaluating to TRUE or FALSE indicating whether NA
          values should be stripped before the computation proceeds.

     ...: further arguments passed to or from other methods.

Value:

     The mean.
"#;

        let args = extract_arguments_from_help(help_text);
        assert_eq!(args.len(), 4);
        assert!(args.get("x").unwrap().contains("An R object."));
        assert!(args.get("x").unwrap().contains("numeric/logical"));
        assert!(args.get("trim").unwrap().contains("fraction"));
        assert!(args.get("trim").unwrap().contains("nearest endpoint"));
        assert!(args.get("na.rm").unwrap().contains("logical"));
        assert!(args.get("na.rm").unwrap().contains("stripped"));
        assert!(args.get("...").unwrap().contains("further arguments"));
    }

    #[test]
    fn test_extract_arguments_no_arguments_section() {
        let help_text = r#"Some Topic

Description:

     Some description.

Usage:

     foo(x)

Value:

     Something.
"#;

        let args = extract_arguments_from_help(help_text);
        assert!(args.is_empty());
    }

    #[test]
    fn test_extract_arguments_empty_input() {
        let args = extract_arguments_from_help("");
        assert!(args.is_empty());
    }

    #[test]
    fn test_extract_arguments_dotted_param_names() {
        // R parameter names can contain dots (e.g., na.rm, use.names)
        let help_text = r#"Test Function

Description:

     A test.

Usage:

     test_fn(na.rm, use.names)

Arguments:

   na.rm: logical. Should missing values be removed?

use.names: logical. Should names be preserved?

Value:

     Result.
"#;

        let args = extract_arguments_from_help(help_text);
        assert!(
            args.contains_key("na.rm"),
            "Should parse dotted param name na.rm"
        );
        assert!(args.get("na.rm").unwrap().contains("missing values"));
    }

    #[test]
    fn test_extract_arguments_curly_quotes_converted() {
        // R help uses Unicode curly quotes for code references
        let help_text = "Test Function\n\
            \n\
            Description:\n\
            \n\
            \x20\x20\x20\x20\x20A test.\n\
            \n\
            Usage:\n\
            \n\
            \x20\x20\x20\x20\x20test_fn(x)\n\
            \n\
            Arguments:\n\
            \n\
            \x20\x20\x20\x20\x20\x20\x20x: An object of class \u{2018}data.frame\u{2019}.\n\
            \n\
            Value:\n\
            \n\
            \x20\x20\x20\x20\x20Result.\n";

        let args = extract_arguments_from_help(help_text);
        assert!(args.contains_key("x"));
        let desc = args.get("x").unwrap();
        assert!(
            desc.contains("`data.frame`"),
            "Curly quotes should be converted to backticks: {desc}"
        );
    }

    #[test]
    fn test_extract_arguments_stops_at_next_section() {
        // The parser should stop at the next section header (e.g., "Value:")
        let help_text = r#"Test

Description:

     A test.

Arguments:

       x: first arg.

       y: second arg.

Value:

     z: this is NOT an argument, it's in the Value section.
"#;

        let args = extract_arguments_from_help(help_text);
        assert_eq!(args.len(), 2);
        assert!(args.contains_key("x"));
        assert!(args.contains_key("y"));
        assert!(
            !args.contains_key("z"),
            "Should not parse entries from Value section"
        );
    }

    // --- Tests for HelpCache::get_arguments ---

    #[test]
    fn test_help_cache_get_arguments_caches_result() {
        let cache = HelpCache::with_max_entries(10);

        // Pre-populate the help text cache with synthetic help text
        let help_text = r#"Test

Description:

     A test.

Arguments:

       x: first arg.

       y: second arg.

Value:

     Result.
"#;
        cache.insert("test_fn", Some("test_pkg"), Some(help_text.to_string()));

        // First call should parse and cache; r_path is required by signature
        // but never invoked since the help text is already in the cache.
        let dummy_r = std::path::Path::new("R");
        let args1 = cache.get_arguments("test_fn", Some("test_pkg"), dummy_r);
        assert!(args1.is_some());
        let args1 = args1.unwrap();
        assert_eq!(args1.len(), 2);
        assert_eq!(args1.get("x").unwrap(), "first arg.");

        // Second call should return cached result (same content)
        let args2 = cache.get_arguments("test_fn", Some("test_pkg"), dummy_r);
        assert!(args2.is_some());
        assert_eq!(args2.unwrap(), args1);
    }

    #[test]
    fn test_help_cache_get_arguments_negative_caching() {
        let cache = HelpCache::with_max_entries(10);

        // Pre-populate with help text that has no Arguments section
        let help_text = "Test\n\nDescription:\n\n     A test.\n\nValue:\n\n     Result.\n";
        cache.insert("no_args_fn", Some("pkg"), Some(help_text.to_string()));

        // Should return None and cache the negative result; r_path never invoked
        // since the help text (which has no Arguments section) is pre-populated.
        let dummy_r = std::path::Path::new("R");
        let args = cache.get_arguments("no_args_fn", Some("pkg"), dummy_r);
        assert!(args.is_none());

        // Verify the negative result is cached (check arguments cache directly)
        {
            let guard = cache.arguments.read().unwrap();
            let key = cache_key("no_args_fn", Some("pkg"));
            let cached = guard.peek(&key);
            assert!(cached.is_some(), "Negative result should be cached");
            assert!(cached.unwrap().is_none(), "Cached value should be None");
        }
    }

    #[test]
    fn test_help_cache_get_arguments_clone_shares_state() {
        // Like the help text cache, the arguments cache must be shared across clones
        let cache1 = HelpCache::with_max_entries(10);
        let cache2 = cache1.clone();

        // Pre-populate help text
        let help_text = "Test\n\nDescription:\n\n     A test.\n\nArguments:\n\n       x: an arg.\n\nValue:\n\n     Result.\n";
        cache1.insert("fn1", Some("pkg"), Some(help_text.to_string()));

        // Fetch arguments through clone; r_path never invoked since cache is pre-populated.
        let dummy_r = std::path::Path::new("R");
        let args = cache2.get_arguments("fn1", Some("pkg"), dummy_r);
        assert!(args.is_some());

        // Verify it's visible through original
        let key = cache_key("fn1", Some("pkg"));
        let guard = cache1.arguments.read().unwrap();
        assert!(
            guard.peek(&key).is_some(),
            "Arguments cached via clone should be visible through original"
        );
    }

    /// Verifies that `get_help` returns `None` when R hangs, and that it does
    /// so well within 1 second.
    ///
    /// Uses a 200 ms timeout (via `RAVEN_HELP_TIMEOUT_MS`) and injects a
    /// `Sys.sleep(60)` snippet via `get_help_with_code`.  The env var is
    /// removed via RAII after the test.
    ///
    /// Marked `#[ignore]` because it mutates a process-wide env var; run explicitly
    /// with `-- --include-ignored --test-threads=1` to avoid interference with
    /// parallel tests.
    #[test]
    #[ignore]
    fn get_help_timeout_returns_none() {
        let r = match crate::r_subprocess::RSubprocess::new(None) {
            Some(s) => s.r_path().clone(),
            None => {
                eprintln!("skip: no R");
                return;
            }
        };
        let _guard = EnvGuard::set("RAVEN_HELP_TIMEOUT_MS", "200");
        let start = std::time::Instant::now();
        let res = get_help_with_code("mean", Some("base"), &r, "Sys.sleep(60)");
        let elapsed = start.elapsed();
        assert!(res.is_none(), "expected None, got {res:?}");
        assert!(
            elapsed.as_millis() < 1000,
            "timeout test took too long: {elapsed:?}"
        );
    }

    /// Paranoia check: after a watchdog-triggered timeout, the R child process must
    /// actually be gone (reaped by the OS).
    ///
    /// After `get_help` returns `None` we send signal 0 (`kill(pid, 0)`) to the
    /// captured PID and assert that it returns -1 with errno ESRCH (no such process).
    /// Signal 0 never delivers — it only probes process existence.
    ///
    /// Marked `#[ignore]` because it mutates a process-wide env var; run explicitly
    /// with `-- --include-ignored --test-threads=1`.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn get_help_timeout_reaps_process() {
        let r = match crate::r_subprocess::RSubprocess::new(None) {
            Some(s) => s.r_path().clone(),
            None => {
                eprintln!("skip: no R");
                return;
            }
        };
        let _guard = EnvGuard::set("RAVEN_HELP_TIMEOUT_MS", "200");

        let pid_capture = AtomicU32::new(0);
        let res = get_help_with_code_capturing_pid(
            "mean",
            Some("base"),
            &r,
            "Sys.sleep(60)",
            &pid_capture,
        );

        assert!(res.is_none(), "expected None (timeout), got {res:?}");

        let pid = pid_capture.load(std::sync::atomic::Ordering::SeqCst);
        assert!(pid != 0, "expected a captured pid after spawn");

        // Brief delay to allow the watchdog kill to take effect before probing.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // SAFETY: signal 0 never delivers; it only checks whether the pid exists.
        let kill_result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(
            kill_result, -1,
            "expected kill(pid, 0) == -1 (process should be reaped); got {}",
            kill_result
        );
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            errno,
            Some(libc::ESRCH),
            "expected ESRCH; got errno {:?}",
            errno
        );
    }

    #[test]
    fn test_help_cache_drain_clears_all_sub_caches() {
        let cache = HelpCache::with_max_entries(10);

        // Populate the main help-text cache (positive entry)
        cache.insert("mean", Some("base"), Some("help text".to_string()));
        assert_eq!(cache.len(), 1);

        // Populate the arguments cache by inserting help text that has an
        // Arguments section and calling get_arguments (which will cache the
        // parsed result); r_path is unused because the help text is already
        // in the main cache.
        let help_text =
            "mean\n\nDescription:\n\n     Mean.\n\nArguments:\n\n       x: numeric vector.\n";
        cache.insert("mean_with_args", Some("base"), Some(help_text.to_string()));
        let dummy_r = std::path::Path::new("R");
        let _ = cache.get_arguments("mean_with_args", Some("base"), dummy_r);
        {
            let guard = cache.arguments.read().unwrap();
            assert_eq!(
                guard.len(),
                1,
                "arguments cache should have one entry before drain"
            );
        }

        // Also insert a negative cache entry
        cache.insert("nonexistent", Some("pkg"), None);

        // Drain everything
        cache.drain();

        // All sub-caches must be empty
        assert_eq!(cache.len(), 0, "inner cache should be empty after drain");
        {
            let guard = cache.arguments.read().unwrap();
            assert_eq!(
                guard.len(),
                0,
                "arguments cache should be empty after drain"
            );
        }

        // Cache misses after drain (not Some(None) for the negative entry)
        assert!(
            cache.get("mean", Some("base")).is_none(),
            "positive entry should be a cache miss after drain"
        );
        assert!(
            cache.get("nonexistent", Some("pkg")).is_none(),
            "negative entry should be a cache miss after drain"
        );
    }
}
