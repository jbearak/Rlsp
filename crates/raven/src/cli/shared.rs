//! Output formatting, severity gating, and file-type helpers shared by the
//! `lint` and `check` subcommands.
//!
//! Both subcommands accumulate `(PathBuf, Diagnostic)` pairs and render them
//! identically (`text` / `json` / `sarif`), gate the process exit code on a
//! `--max-severity` threshold, and agree on which files are R sources versus
//! chunk-bearing documents. That common surface lives here so the two commands
//! share one implementation without depending on each other's internals.

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use serde_json::json;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

/// Exit codes are returned as plain `i32` so `main()` can pass them directly
/// to `std::process::exit`. Avoids the `ExitCode` cast trap (`ExitCode` is not
/// a primitive and cannot be cast with `as`).
pub const EXIT_OK: i32 = 0;
pub const EXIT_LINT_FAILED: i32 = 1;
pub const EXIT_OPERATOR_ERROR: i32 = 2;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum OutputFormat {
    Text,
    Json,
    Sarif,
}

/// Tri-state color control for the `text` renderer, mirroring `cargo`/`ripgrep`.
/// `--no-color` is kept as an alias for `Never`. The choice is resolved to a
/// concrete on/off by [`resolve_color`]; the CLI entrypoints reach it through
/// [`resolve_color_from_env`], which reads `NO_COLOR` / `FORCE_COLOR` and TTY
/// state and applies them under `Auto`.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, PartialEq, Clone, Copy, PartialOrd, Eq, Ord)]
pub enum SeverityLevel {
    Off,
    Hint,
    Info,
    Warning,
    Error,
}

impl SeverityLevel {
    pub fn from_diag(d: &Diagnostic) -> Self {
        match d.severity {
            Some(DiagnosticSeverity::ERROR) => SeverityLevel::Error,
            Some(DiagnosticSeverity::WARNING) => SeverityLevel::Warning,
            Some(DiagnosticSeverity::INFORMATION) => SeverityLevel::Info,
            Some(DiagnosticSeverity::HINT) => SeverityLevel::Hint,
            _ => SeverityLevel::Off,
        }
    }
}

/// Parse a `--format` value into an [`OutputFormat`].
pub fn parse_output_format(v: &str) -> Result<OutputFormat, String> {
    match v {
        "text" => Ok(OutputFormat::Text),
        "json" => Ok(OutputFormat::Json),
        "sarif" => Ok(OutputFormat::Sarif),
        other => Err(format!("unknown --format value: {other}")),
    }
}

/// Parse a `--max-severity` value into a [`SeverityLevel`].
pub fn parse_severity_level(v: &str) -> Result<SeverityLevel, String> {
    match v {
        "off" => Ok(SeverityLevel::Off),
        "hint" => Ok(SeverityLevel::Hint),
        "info" => Ok(SeverityLevel::Info),
        "warning" => Ok(SeverityLevel::Warning),
        "error" => Ok(SeverityLevel::Error),
        other => Err(format!("unknown --max-severity value: {other}")),
    }
}

/// Parse a `--color` value into a [`ColorChoice`].
pub fn parse_color_choice(v: &str) -> Result<ColorChoice, String> {
    match v {
        "auto" => Ok(ColorChoice::Auto),
        "always" => Ok(ColorChoice::Always),
        "never" => Ok(ColorChoice::Never),
        other => Err(format!("unknown --color value: {other}")),
    }
}

/// Whether an environment variable counts as "set" for color control. Per
/// no-color.org / force-color.org, that means present **and non-empty** —
/// regardless of UTF-8 validity, which is why the value is an `OsStr` (a
/// non-UTF-8 but non-empty `NO_COLOR` still counts; `std::env::var` would have
/// dropped it). Reading the env stays at the call site (see
/// [`resolve_color_from_env`]); this predicate is pure so the rule is
/// unit-testable without racing process-global env across parallel tests.
pub fn env_flag_is_set(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Resolve a [`ColorChoice`] to a concrete on/off, applying the precedence from
/// no-color.org: an explicit `Always`/`Never` (including the `--no-color` alias)
/// always wins, overriding the environment. Only under `Auto` do the env signals
/// apply — `no_color` forces off, then `force_color` forces on, otherwise color
/// follows whether stdout is a terminal. The two flags are "is the var set?"
/// booleans (see [`env_flag_is_set`]), so this stays a pure precedence function
/// over already-resolved inputs — unit-testable without touching the real env.
pub fn resolve_color(
    choice: ColorChoice,
    no_color: bool,
    force_color: bool,
    stdout_is_tty: bool,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if no_color {
                false
            } else if force_color {
                true
            } else {
                stdout_is_tty
            }
        }
    }
}

/// Resolve a [`ColorChoice`] against the live process environment — the impure
/// counterpart to [`resolve_color`], which it delegates to after reading
/// `NO_COLOR`/`FORCE_COLOR` and probing stdout for a TTY. Both `check` and
/// `lint` call this once in their `run()` epilogue so the env+TTY gathering
/// lives in one place; the pure `resolve_color` stays separate for unit tests.
pub fn resolve_color_from_env(choice: ColorChoice) -> bool {
    resolve_color(
        choice,
        env_flag_is_set(std::env::var_os("NO_COLOR").as_deref()),
        env_flag_is_set(std::env::var_os("FORCE_COLOR").as_deref()),
        std::io::stdout().is_terminal(),
    )
}

pub fn is_r_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|s| s.to_str()),
        Some("R") | Some("r")
    )
}

/// True for `.stan`, matched case-insensitively to agree with Raven's
/// canonical file-type classifier. `.stanfunctions` is intentionally excluded.
pub fn is_stan_file(p: &Path) -> bool {
    p.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("stan"))
}

/// True for standalone `.jags` and `.bugs` programs, matched
/// case-insensitively. Singular `.bug` is not a supported model extension.
pub fn is_jags_file(p: &Path) -> bool {
    p.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("jags") || extension.eq_ignore_ascii_case("bugs")
        })
}

/// True for chunk-bearing document extensions (`.Rmd` / `.Rmarkdown` / `.qmd`),
/// matched case-insensitively so the CLI walk agrees with the canonical
/// [`crate::chunks::classify_chunk_document`] (which lowercases the whole
/// path) — a `report.rMd` saved on a case-insensitive filesystem must be
/// collected by the same rule that later classifies it.
pub fn is_chunk_file(p: &Path) -> bool {
    crate::chunks::classify_chunk_document(&p.to_string_lossy()) == crate::chunks::ChunkKind::Rmd
}

/// Return `path` unchanged when already absolute; otherwise resolve it against
/// `base`.
pub fn absolute_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Recursively collect `.R` / `.r` file paths under `dir`. A thin R-only
/// wrapper over the shared directory walk `crate::state::collect_files_matching`:
/// symlinked directories are followed with canonical-path cycle detection and
/// non-source directories are pruned. Reusing the indexer's walk is what keeps
/// `raven check`'s *reported* file set equal to its *indexed* set — a `.R` file
/// reachable only through a symlink (e.g. a monorepo `src -> ../shared` layout)
/// is both indexed and reported, not one or the other. Results are unsorted;
/// callers that need deterministic order sort afterwards.
///
/// Shared by `raven check` (which reports diagnostics for the collected files)
/// and `analysis-stats` (which reads their contents in a second pass). `.r` and
/// `.R` are the only matched extensions — equivalent to a case-insensitive
/// match on the single-character extension.
pub fn collect_r_file_paths(dir: &Path, out: &mut Vec<PathBuf>) {
    crate::state::collect_files_matching(dir, out, is_r_file);
}

pub fn collect_r_file_paths_with_exclusions(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) {
    crate::state::collect_files_matching_with_exclusions(dir, out, is_r_file, exclusions);
}

/// Recursively collect R sources, chunk-bearing documents, and standalone JAGS
/// or Stan models under `dir` (all extension checks are case-insensitive). Same
/// directory walk as [`collect_r_file_paths`] — symlinked directories are
/// followed with canonical-path cycle detection and non-source directories
/// pruned. Chunk documents are included so their R chunks are diagnosed (issue
/// #343); singular `.bug` and Stan include fragments are not standalone targets.
///
/// Used by `raven check`'s report walk (empty `PATHS` or an explicit
/// directory). Results are unsorted; callers that need deterministic order
/// sort afterwards.
pub fn collect_check_target_paths(dir: &Path, out: &mut Vec<PathBuf>) {
    crate::state::collect_files_matching(dir, out, |p| {
        is_r_file(p) || is_chunk_file(p) || is_jags_file(p) || is_stan_file(p)
    });
}

pub fn collect_check_target_paths_with_exclusions(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) {
    crate::state::collect_files_matching_with_exclusions(
        dir,
        out,
        |p| is_r_file(p) || is_chunk_file(p) || is_jags_file(p) || is_stan_file(p),
        exclusions,
    );
}

/// Build the reported finding for a target that isn't valid UTF-8. A
/// mis-encoded source file (typically Latin-1 / Windows-1252 saved without a
/// BOM) can't be parsed, but it's a property of the user's code — so it's an
/// error-severity diagnostic (exit 1, like a syntax error) rather than an
/// operator error (exit 2). The range is `0:0` because the file can't be
/// decoded into lines; the byte offset in the message is the actionable
/// pointer. `byte == 0` marks the rare malformed-UTF-16 case, where there's no
/// single offending byte to name.
///
/// Shared by `raven check` (report loop) and `raven lint` (`walk`) so the two
/// emit the identical message for an undecodable `crate::state::read_source`
/// failure.
pub fn encoding_diagnostic(offset: usize, byte: u8) -> Diagnostic {
    use tower_lsp::lsp_types::{Position, Range};
    // Compose the whole message per branch rather than gluing a fixed
    // "not valid UTF-8" prefix onto the detail: the `byte == 0` case can be a
    // malformed UTF-16 file (which carried a BOM), so a "not valid UTF-8" prefix
    // would contradict its own tail.
    let message = if byte == 0 {
        // No single offending byte to name: a truncated multibyte sequence at
        // EOF, or malformed/odd-length UTF-16.
        "File could not be decoded as UTF-8 or UTF-16. Re-save the file as UTF-8.".to_string()
    } else {
        format!(
            "File is not valid UTF-8: first invalid byte {byte:#04x} at offset {offset} \
             (looks like Latin-1/Windows-1252). Re-save the file as UTF-8."
        )
    };
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        message,
        ..Default::default()
    }
}

/// Wrap a severity word in a minimal ANSI color span when `use_color` is set,
/// returning it unchanged otherwise. Only the severity word is colorized (per
/// the design); the path, location, message, and `[rule]` stay uncolored so the
/// output remains easy to grep. `note` (severity-less / unrecognized) is left
/// uncolored intentionally.
///
/// The colored forms are a closed set of `&'static str` literals (color code +
/// word + `\x1b[0m` reset), so this allocates nothing and returns a borrow — the
/// uncolored cases return the input `level` unchanged.
fn colorize_level(level: &str, use_color: bool) -> &str {
    if !use_color {
        return level;
    }
    match level {
        "error" => "\x1b[31merror\x1b[0m",     // red
        "warning" => "\x1b[33mwarning\x1b[0m", // yellow
        "info" => "\x1b[34minfo\x1b[0m",       // blue
        "hint" => "\x1b[36mhint\x1b[0m",       // cyan
        _ => level,
    }
}

/// Render diagnostics in the requested format. The single dispatch point both
/// `lint` and `check` call after collecting their `(path, diagnostic)` pairs,
/// so the format → renderer mapping lives in one place next to the renderers.
/// `use_color` colorizes only the `text` renderer's severity word; `json` /
/// `sarif` are machine formats and never colorized regardless.
pub fn render(
    format: OutputFormat,
    diags: &[(PathBuf, Diagnostic)],
    root: &Path,
    quiet: bool,
    use_color: bool,
) {
    match format {
        OutputFormat::Text => print_text(diags, root, quiet, use_color),
        OutputFormat::Json => print_json(diags, root),
        OutputFormat::Sarif => print_sarif(diags, root),
    }
}

fn print_text(diags: &[(PathBuf, Diagnostic)], root: &Path, quiet: bool, use_color: bool) {
    let mut errors = 0;
    let mut warnings = 0;
    let mut infos = 0;
    let mut hints = 0;
    let mut notes = 0;
    for (path, d) in diags {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let level = match d.severity {
            Some(DiagnosticSeverity::ERROR) => {
                errors += 1;
                "error"
            }
            Some(DiagnosticSeverity::WARNING) => {
                warnings += 1;
                "warning"
            }
            Some(DiagnosticSeverity::INFORMATION) => {
                infos += 1;
                "info"
            }
            Some(DiagnosticSeverity::HINT) => {
                hints += 1;
                "hint"
            }
            _ => {
                notes += 1;
                "note"
            }
        };
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        let rule = match &d.code {
            Some(NumberOrString::String(s)) => s.as_str(),
            _ => "",
        };
        println!(
            "{}:{}:{} {}: {} [{}]",
            rel.display(),
            line,
            col,
            colorize_level(level, use_color),
            d.message,
            rule
        );
    }
    if !quiet {
        // Buckets sum to diags.len(): errors + warnings + infos + hints
        // + notes (severity-less / unrecognized). Per-bucket reporting
        // keeps INFORMATION distinct from WARNING in summaries — SARIF
        // collapses them onto "note" by spec, but the human-readable
        // CLI output should reflect the original LSP severity.
        println!(
            "{} issues ({} errors, {} warnings, {} infos, {} hints, {} notes)",
            diags.len(),
            errors,
            warnings,
            infos,
            hints,
            notes
        );
    }
}

fn print_json(diags: &[(PathBuf, Diagnostic)], root: &Path) {
    let arr: Vec<_> = diags
        .iter()
        .map(|(p, d)| {
            let rel = p.strip_prefix(root).unwrap_or(p);
            // Strip the internal NSE hint from `data` (text-footer only — see
            // `data_without_nse_hint`) so it never reaches machine output. Clone
            // so the in-memory diagnostics the footer later reads keep their
            // hint.
            let mut d = d.clone();
            d.data = crate::diagnostic_code::data_without_nse_hint(&d.data);
            json!({ "path": rel.display().to_string(), "diagnostic": d })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json!(arr)).unwrap());
}

fn print_sarif(diags: &[(PathBuf, Diagnostic)], root: &Path) {
    use std::collections::BTreeSet;
    let rule_ids: BTreeSet<String> = diags
        .iter()
        .filter_map(|(_, d)| match &d.code {
            Some(NumberOrString::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let rules: Vec<_> = rule_ids
        .iter()
        .map(|id| {
            json!({
                "id": id, "name": id, "shortDescription": { "text": id }
            })
        })
        .collect();
    let results: Vec<_> = diags
        .iter()
        .map(|(p, d)| {
            let rel = p.strip_prefix(root).unwrap_or(p);
            let level = match d.severity {
                Some(DiagnosticSeverity::ERROR) => "error",
                Some(DiagnosticSeverity::WARNING) => "warning",
                _ => "note",
            };
            let rule_id = match &d.code {
                Some(NumberOrString::String(s)) => s.clone(),
                _ => String::new(),
            };
            json!({
                "ruleId": rule_id,
                "level": level,
                "message": { "text": d.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": rel.display().to_string() },
                        "region": {
                            "startLine": d.range.start.line + 1,
                            "startColumn": d.range.start.character + 1,
                            "endLine": d.range.end.line + 1,
                            "endColumn": d.range.end.character + 1,
                        }
                    }
                }]
            })
        })
        .collect();
    let sarif = json!({
        "version": "2.1.0",
        "$schema": "https://docs.oasis-open.org/sarif/sarif/v2.1.0/cos02/schemas/sarif-schema-2.1.0.json",
        "runs": [{
            "tool": { "driver": { "name": "raven", "rules": rules } },
            "results": results
        }]
    });
    println!("{}", serde_json::to_string_pretty(&sarif).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, Range};

    fn diag(severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(0, 1),
            },
            severity: Some(severity),
            ..Default::default()
        }
    }

    #[test]
    fn encoding_diagnostic_message_matches_the_failure_kind() {
        // Latin-1 (a concrete offending byte): names UTF-8 and the byte/offset.
        let latin1 = encoding_diagnostic(930, 0xA0);
        assert!(
            latin1.message.contains("not valid UTF-8") && latin1.message.contains("0xa0"),
            "{}",
            latin1.message
        );
        // Malformed/odd-length UTF-16 (byte == 0): the file carried a BOM, so the
        // message must NOT claim "not valid UTF-8" and must mention UTF-16.
        let utf16 = encoding_diagnostic(0, 0);
        assert!(
            !utf16.message.contains("not valid UTF-8"),
            "{}",
            utf16.message
        );
        assert!(utf16.message.contains("UTF-16"), "{}", utf16.message);
    }

    #[test]
    fn severity_ordering() {
        assert!(SeverityLevel::Error > SeverityLevel::Warning);
        assert!(SeverityLevel::Warning > SeverityLevel::Info);
        assert!(SeverityLevel::Info > SeverityLevel::Hint);
        assert!(SeverityLevel::Hint > SeverityLevel::Off);
    }

    #[test]
    fn severity_from_diag() {
        assert_eq!(
            SeverityLevel::from_diag(&diag(DiagnosticSeverity::ERROR)),
            SeverityLevel::Error
        );
        assert_eq!(
            SeverityLevel::from_diag(&diag(DiagnosticSeverity::WARNING)),
            SeverityLevel::Warning
        );
        assert_eq!(
            SeverityLevel::from_diag(&diag(DiagnosticSeverity::INFORMATION)),
            SeverityLevel::Info
        );
        assert_eq!(
            SeverityLevel::from_diag(&diag(DiagnosticSeverity::HINT)),
            SeverityLevel::Hint
        );
    }

    #[test]
    fn format_parsing() {
        assert_eq!(parse_output_format("text").unwrap(), OutputFormat::Text);
        assert_eq!(parse_output_format("json").unwrap(), OutputFormat::Json);
        assert_eq!(parse_output_format("sarif").unwrap(), OutputFormat::Sarif);
        assert!(parse_output_format("toml").is_err());
    }

    #[test]
    fn severity_parsing() {
        assert_eq!(parse_severity_level("off").unwrap(), SeverityLevel::Off);
        assert_eq!(parse_severity_level("error").unwrap(), SeverityLevel::Error);
        assert!(parse_severity_level("fatal").is_err());
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color_choice("auto").unwrap(), ColorChoice::Auto);
        assert_eq!(parse_color_choice("always").unwrap(), ColorChoice::Always);
        assert_eq!(parse_color_choice("never").unwrap(), ColorChoice::Never);
        assert!(parse_color_choice("yes").is_err());
    }

    #[test]
    fn env_flag_is_set_requires_present_and_non_empty() {
        assert!(!env_flag_is_set(None), "unset ⇒ not set");
        assert!(!env_flag_is_set(Some(OsStr::new(""))), "empty ⇒ not set");
        assert!(env_flag_is_set(Some(OsStr::new("1"))), "non-empty ⇒ set");
    }

    /// A non-UTF-8 but non-empty value must still count as set (no-color.org
    /// cares only about presence + non-emptiness). This is the case `var_os`
    /// preserves and `var().ok()` would silently drop to "unset".
    #[cfg(unix)]
    #[test]
    fn env_flag_is_set_treats_non_utf8_value_as_set() {
        use std::os::unix::ffi::OsStrExt;
        let invalid = OsStr::from_bytes(&[0xFF]); // not valid UTF-8, non-empty
        assert!(env_flag_is_set(Some(invalid)));
    }

    #[test]
    fn resolve_color_explicit_overrides_win_over_env_and_tty() {
        // `always` forces on even when piped (no TTY) and NO_COLOR is set.
        assert!(resolve_color(ColorChoice::Always, true, false, false));
        // `never` forces off even on a TTY with FORCE_COLOR set.
        assert!(!resolve_color(ColorChoice::Never, false, true, true));
    }

    #[test]
    fn resolve_color_auto_honors_no_color() {
        // NO_COLOR set disables color even on a TTY.
        assert!(!resolve_color(ColorChoice::Auto, true, false, true));
        // NO_COLOR unset falls through to TTY detection.
        assert!(resolve_color(ColorChoice::Auto, false, false, true));
        // NO_COLOR beats FORCE_COLOR when both are set.
        assert!(!resolve_color(ColorChoice::Auto, true, true, true));
    }

    #[test]
    fn resolve_color_auto_honors_force_color_then_tty() {
        // FORCE_COLOR forces on even when piped.
        assert!(resolve_color(ColorChoice::Auto, false, true, false));
        // No env overrides: color follows the TTY flag.
        assert!(resolve_color(ColorChoice::Auto, false, false, true));
        assert!(!resolve_color(ColorChoice::Auto, false, false, false));
    }

    #[test]
    fn colorize_level_wraps_only_when_enabled() {
        // Disabled: returned verbatim, no escapes.
        assert_eq!(colorize_level("error", false), "error");
        // Enabled: wrapped in an SGR span that resets at the end.
        let colored = colorize_level("error", true);
        assert!(colored.starts_with("\x1b["), "{colored}");
        assert!(colored.ends_with("\x1b[0m"), "{colored}");
        assert!(colored.contains("error"), "{colored}");
        // `note` is intentionally left uncolored even when enabled.
        assert_eq!(colorize_level("note", true), "note");
    }

    #[test]
    fn file_type_predicates() {
        assert!(is_r_file(Path::new("a.R")));
        assert!(is_r_file(Path::new("a.r")));
        assert!(!is_r_file(Path::new("a.Rmd")));
        assert!(is_chunk_file(Path::new("a.Rmd")));
        assert!(is_chunk_file(Path::new("a.Rmarkdown")));
        assert!(is_chunk_file(Path::new("a.rmarkdown")));
        assert!(is_chunk_file(Path::new("a.RMARKDOWN")));
        assert!(is_chunk_file(Path::new("a.qmd")));
        // Match classify_chunk_document's suffix semantics exactly: no-stem
        // dotfile names still end with the chunk-document suffix.
        assert!(is_chunk_file(Path::new(".Rmd")));
        assert!(is_chunk_file(Path::new(".Rmarkdown")));
        assert!(is_chunk_file(Path::new(".qmd")));
        // Case-insensitive, matching classify_chunk_document — including
        // capitalizations outside the common Rmd/rmd/RMD set.
        assert!(is_chunk_file(Path::new("a.rMd")));
        assert!(is_chunk_file(Path::new("a.rMarkDown")));
        assert!(is_chunk_file(Path::new("a.QmD")));
        assert!(!is_chunk_file(Path::new("a.R")));
        assert!(!is_chunk_file(Path::new("a.txt")));
        assert!(!is_chunk_file(Path::new("a.rmdx")));
        assert!(is_stan_file(Path::new("a.stan")));
        assert!(is_stan_file(Path::new("a.StAn")));
        assert!(!is_stan_file(Path::new("a.stanfunctions")));
        assert!(is_jags_file(Path::new("a.jags")));
        assert!(is_jags_file(Path::new("a.JaGs")));
        assert!(is_jags_file(Path::new("a.bugs")));
        assert!(is_jags_file(Path::new("a.BuGs")));
        assert!(!is_jags_file(Path::new("a.bug")));
    }

    #[test]
    fn collect_r_file_paths_walks_and_prunes() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("a.R"), "1\n").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/b.r"), "2\n").unwrap();
        fs::write(tmp.path().join("c.Rmd"), "prose\n").unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".git/d.R"), "3\n").unwrap();

        let mut out = Vec::new();
        collect_r_file_paths(tmp.path(), &mut out);
        // a.R + sub/b.r; .Rmd is not an R source; .git is pruned.
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|p| is_r_file(p)));
    }

    #[test]
    fn collect_check_target_paths_includes_r_and_chunk_files() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("a.R"), "1\n").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/b.r"), "2\n").unwrap();
        fs::write(tmp.path().join("c.Rmd"), "prose\n").unwrap();
        fs::write(tmp.path().join("d.Rmarkdown"), "prose\n").unwrap();
        fs::write(tmp.path().join("e.qmd"), "prose\n").unwrap();
        fs::write(tmp.path().join(".Rmarkdown"), "prose\n").unwrap();
        // Mixed-case chunk extensions are matched (is_chunk_file is
        // case-insensitive).
        fs::write(tmp.path().join("f.QMD"), "prose\n").unwrap();
        fs::write(tmp.path().join("g.RMARKDOWN"), "prose\n").unwrap();
        fs::write(tmp.path().join("model.stan"), "model {}\n").unwrap();
        fs::write(tmp.path().join("mixed.StAn"), "model {}\n").unwrap();
        fs::write(tmp.path().join("model.jags"), "model { x <- 1 }\n").unwrap();
        fs::write(tmp.path().join("mixed.JAGS"), "model { x <- 1 }\n").unwrap();
        fs::write(tmp.path().join("legacy.bugs"), "model { x <- * 1 }\n").unwrap();
        fs::write(tmp.path().join("mixed.BUGS"), "model { x <- 1 }\n").unwrap();
        fs::write(tmp.path().join("unsupported.bug"), "model { x <- * 1 }\n").unwrap();
        fs::write(tmp.path().join("helper.stanfunctions"), "functions {}\n").unwrap();
        fs::write(tmp.path().join("f.txt"), "not source\n").unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".git/g.R"), "3\n").unwrap();

        let mut out = Vec::new();
        collect_check_target_paths(tmp.path(), &mut out);
        // a.R + sub/b.r + c.Rmd + d.Rmarkdown + e.qmd + .Rmarkdown + f.QMD +
        // g.RMARKDOWN; .txt skipped; .git pruned.
        assert_eq!(out.len(), 14, "got {out:?}");
        assert!(
            out.iter()
                .all(|p| is_r_file(p) || is_chunk_file(p) || is_jags_file(p) || is_stan_file(p))
        );
        assert!(out.iter().any(|p| is_chunk_file(p)));
        assert_eq!(out.iter().filter(|p| is_stan_file(p)).count(), 2);
        assert_eq!(out.iter().filter(|p| is_jags_file(p)).count(), 4);
        assert!(out.iter().any(|path| path.ends_with("legacy.bugs")));
        assert!(out.iter().any(|path| path.ends_with("mixed.BUGS")));
        assert!(!out.iter().any(|path| path.ends_with("unsupported.bug")));
    }

    #[cfg(unix)]
    #[test]
    fn collect_r_file_paths_follows_symlinks_with_cycle_detection() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("real.R"), "1\n").unwrap();
        // A symlinked .R file is collected — the workspace indexer
        // (state.rs collect_file_paths_inner) includes it, so the report set
        // must too or `raven check` exits clean over a file the editor flags.
        std::os::unix::fs::symlink(tmp.path().join("real.R"), tmp.path().join("link.R")).unwrap();
        // Files reached only via a symlinked directory MUST be collected — the
        // monorepo `src -> ../shared` layout this guards against.
        fs::create_dir(tmp.path().join("d")).unwrap();
        fs::write(tmp.path().join("d/inner.R"), "2\n").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("d"), tmp.path().join("dlink")).unwrap();
        // A self-referential symlink must terminate (cycle guard), not recurse
        // forever.
        std::os::unix::fs::symlink(tmp.path(), tmp.path().join("selfloop")).unwrap();

        let mut out = Vec::new();
        collect_r_file_paths(tmp.path(), &mut out);

        // real.R + link.R + inner.R: `inner.R` is reached exactly once (the
        // canonical-path cycle guard prevents visiting both `d` and `dlink`),
        // and `selfloop` is skipped. Termination itself proves the guard works.
        assert_eq!(out.len(), 3, "got {out:?}");
        let canon_inner = fs::canonicalize(tmp.path().join("d/inner.R")).unwrap();
        assert!(
            out.iter()
                .any(|p| fs::canonicalize(p).unwrap() == canon_inner),
            "the file under the symlinked directory must be collected; got {out:?}"
        );
    }
}
