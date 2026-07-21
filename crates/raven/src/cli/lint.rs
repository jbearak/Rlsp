//! `raven lint` subcommand: walk paths, run native lint rules, format output.

use std::path::{Path, PathBuf};

use serde_json::json;
use tower_lsp::lsp_types::Diagnostic;

use crate::cli::shared::{
    ColorChoice, EXIT_LINT_FAILED, EXIT_OK, EXIT_OPERATOR_ERROR, OutputFormat, SeverityLevel,
    absolute_path, encoding_diagnostic, is_chunk_file, is_r_file, parse_color_choice,
    parse_output_format, parse_severity_level, render, resolve_color_from_env,
};

#[derive(Debug, PartialEq, Clone)]
pub struct LintArgs {
    pub paths: Vec<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub no_config: bool,
    pub format: OutputFormat,
    pub max_severity: SeverityLevel,
    pub quiet: bool,
    /// Color control for `text` output. `--no-color` parses to
    /// [`ColorChoice::Never`]; `--color auto|always|never` sets it directly.
    /// Resolved to on/off by [`resolve_color_from_env`] (TTY +
    /// `NO_COLOR`/`FORCE_COLOR`).
    pub color: ColorChoice,
}

pub fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<LintArgs, String> {
    let mut paths = Vec::new();
    let mut config_path = None;
    let mut no_config = false;
    let mut format = OutputFormat::Text;
    let mut max_severity = SeverityLevel::Info;
    let mut quiet = false;
    // `--color` and `--no-color` write the same field; last-one-wins on conflict
    // (`--no-color --color always` ⇒ always), matching cargo/ripgrep.
    let mut color = ColorChoice::Auto;

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(argv.next().ok_or("--config needs a path")?));
            }
            "--no-config" => no_config = true,
            "--format" => {
                let v = argv.next().ok_or("--format needs a value")?;
                format = parse_output_format(&v)?;
            }
            "--max-severity" => {
                let v = argv.next().ok_or("--max-severity needs a value")?;
                max_severity = parse_severity_level(&v)?;
            }
            "--quiet" => quiet = true,
            "--color" => {
                let v = argv.next().ok_or("--color needs a value")?;
                color = parse_color_choice(&v)?;
            }
            "--no-color" => color = ColorChoice::Never,
            "--help" => return Err("HELP".into()),
            s if s.starts_with("--") => return Err(format!("unknown flag: {s}")),
            p => paths.push(PathBuf::from(p)),
        }
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    Ok(LintArgs {
        paths,
        config_path,
        no_config,
        format,
        max_severity,
        quiet,
        color,
    })
}

pub fn print_help() {
    println!(
        "raven lint {} — native R style linter

Usage: raven lint [OPTIONS] [PATHS...]

Lints each .R / .r file against the rules configured in raven.toml
(or .lintr) and prints diagnostics. R Markdown / Quarto files
(.Rmd / .Rmarkdown / .qmd) are linted too: the R code inside chunks is checked,
prose and non-R chunks are ignored.

Options:
  --config PATH               Path to raven.toml or .lintr (default: search upward,
                              skipping literal ~/.lintr unless passed here)
  --no-config                 Use built-in defaults; ignore raven.toml/.lintr
  --format text|json|sarif    Output format (default: text)
  --max-severity LEVEL        Highest severity that does NOT fail the build
                              (off, hint, info, warning, error; default: info)
  --quiet                     Suppress summary line in text output
  --color auto|always|never   When to colorize text output (default: auto —
                              color when stdout is a terminal). Honors NO_COLOR
                              and FORCE_COLOR under auto; json/sarif are never
                              colorized.
  --no-color                  Alias for --color never

Exit codes:
  0   No diagnostic exceeded --max-severity
  1   A diagnostic exceeded --max-severity, or a usage error (unknown flag / bad option value)
  2   Operator error while running (config parse failure, unreadable or missing path)
",
        env!("CARGO_PKG_VERSION")
    );
}

/// Resolve the project root, project settings, and lintr-discovered signal for
/// `raven lint`. Factored out of [`run`] and parameterized on `cwd` so the
/// discover-and-load branch is unit-testable without mutating the
/// process-global current directory.
///
/// Precedence matches the editor: `--no-config` wins, then an explicit
/// `--config`, then auto-discovery via the shared
/// [`crate::config_file::discover_and_load`] seam. Routing `raven lint` through
/// that seam — the same one the LSP startup path, the watched-files reload, and
/// `raven check` use — keeps the four from drifting on discovery precedence or
/// which loader reads `.lintr`.
///
/// `lintr_discovered` is the input to `Auto` resolution in
/// [`crate::backend::parse_lint_config`]: true only when a `.lintr` (not a
/// `raven.toml`) is the discovered config. [`crate::config_file::find_config`]
/// names the discovered file `.lintr` or `raven.toml`, so its file name is a
/// reliable toml-vs-lintr discriminator — the same derivation
/// `build_project_config_loaded_payload` uses for its notification payload.
///
/// Warnings go to stderr. Returns `Err(EXIT_OPERATOR_ERROR)` for an
/// unreadable/unparseable config (the explicit `--config` and discovery paths
/// print a one-line note first); `Ok((root, project_settings, lintr_discovered))`
/// otherwise.
#[cfg(test)]
fn resolve_lint_config(
    cwd: &Path,
    args: &LintArgs,
) -> Result<(PathBuf, Option<serde_json::Value>, bool), i32> {
    resolve_lint_config_with_options(cwd, args, &crate::config_file::DiscoveryOptions::default())
}

fn resolve_lint_config_with_options(
    cwd: &Path,
    args: &LintArgs,
    discovery_options: &crate::config_file::DiscoveryOptions,
) -> Result<(PathBuf, Option<serde_json::Value>, bool), i32> {
    if args.no_config {
        return Ok((cwd.to_path_buf(), None, false));
    }

    if let Some(explicit) = args.config_path.as_ref() {
        return match crate::config_file::load_explicit_config_from_base(cwd, explicit) {
            Some((explicit_abs, l)) => {
                for w in l.warnings {
                    eprintln!("{w}");
                }
                let is_lintr = crate::config_file::ConfigFileKind::is_lintr_path(&explicit_abs);
                // `raven.toml` and project-local `.lintr` files are
                // project-root configs, so an explicit path anchors the lint
                // root at the config parent. Literal home `.lintr` is the
                // exception: `--config ~/.lintr` is a user-level opt-in, so keep
                // paths relative to the invocation cwd and lint that project
                // rather than `$HOME`.
                let base =
                    if is_lintr && discovery_options.is_home_lintr_path(explicit_abs.as_path()) {
                        cwd.to_path_buf()
                    } else {
                        // Resolve the parent so `root` is absolute even when
                        // `--config` points at a relative path. Without this,
                        // `resolve_lint_for_document`'s `strip_prefix(root)` check
                        // fails for the absolute URIs produced by `walk` — silently
                        // dropping every per-file `[[linting.overrides]]` patch
                        // (same failure mode as commit 81978f0 fixed for the
                        // non-explicit root).
                        explicit_abs
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| cwd.to_path_buf())
                    };
                // Normalize away `.`/`..`: `strip_prefix(root)` is purely
                // lexical, so a `..` left in `root` (e.g. `--config
                // ../pkg/raven.toml`) wouldn't prefix-match a file given by its
                // absolute path under that config root, again silently dropping
                // its overrides. Normalize lexically (not via `canonicalize`)
                // so a non-existent root still resolves predictably.
                let root = crate::cross_file::normalize_path_public(&base).unwrap_or(base);
                // A `.lintr` (even one passed explicitly) opts into linting only
                // when it expresses linting config; a blank one must not. Shared
                // policy with the server gate (see `lintr_path_opts_in`).
                let lintr_discovered =
                    crate::config_file::lintr_path_opts_in(&explicit_abs, Some(&l.settings));
                Ok((root, Some(l.settings), lintr_discovered))
            }
            None => {
                eprintln!("raven lint: failed to load --config {}", explicit.display());
                Err(EXIT_OPERATOR_ERROR)
            }
        };
    }

    match crate::config_file::discover_and_load_with_options(cwd, discovery_options) {
        crate::config_file::DiscoveredLoad::Loaded {
            path,
            settings,
            warnings,
        } => {
            for w in warnings {
                eprintln!("{w}");
            }
            // A discovered `.lintr` opts into linting only when it expresses
            // linting config; a blank/empty one must not (shared policy with the
            // server's `recompute_parsed_configs` gate, see `lintr_path_opts_in`).
            let lintr_discovered = crate::config_file::lintr_path_opts_in(&path, Some(&settings));
            let root = path.parent().unwrap_or(cwd).to_path_buf();
            Ok((root, Some(settings), lintr_discovered))
        }
        crate::config_file::DiscoveredLoad::LoadFailed { path } => {
            eprintln!("raven lint: failed to load {}", path.display());
            Err(EXIT_OPERATOR_ERROR)
        }
        crate::config_file::DiscoveredLoad::None => Ok((cwd.to_path_buf(), None, false)),
    }
}

pub fn run(args: LintArgs) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("raven lint: cannot read current directory: {e}");
            return EXIT_OPERATOR_ERROR;
        }
    };
    run_with_cwd(args, &cwd)
}

fn run_with_cwd(args: LintArgs, cwd: &Path) -> i32 {
    run_with_cwd_and_options(args, cwd, &crate::config_file::DiscoveryOptions::default())
}

fn run_with_cwd_and_options(
    args: LintArgs,
    cwd: &Path,
    discovery_options: &crate::config_file::DiscoveryOptions,
) -> i32 {
    // Resolve project root + project settings + lintr-discovered signal.
    let (root, project_settings, lintr_discovered) =
        match resolve_lint_config_with_options(cwd, &args, discovery_options) {
            Ok(v) => v,
            Err(code) => return code,
        };

    // Parse the base lint config from the (project-only) settings, since the
    // CLI has no LSP client. Merge with an empty client layer for correctness.
    let mut merged = crate::config_file::merge_settings(
        &serde_json::Value::Object(Default::default()),
        project_settings.as_ref(),
    );
    force_cli_linting_enabled(&mut merged);
    let lint_config =
        crate::backend::parse_lint_config(&merged, lintr_discovered).unwrap_or_default();
    let base_section = merged.get("linting").cloned().unwrap_or(json!({}));
    let overrides = crate::config_file::compile_lint_overrides(&merged, &root);

    let mut diagnostics: Vec<(PathBuf, Diagnostic)> = Vec::new();
    let mut operator_error = false;
    for p in args.paths.iter().map(|p| {
        if p.is_absolute() {
            p.clone()
        } else {
            cwd.join(p)
        }
    }) {
        walk(
            &p,
            &root,
            &base_section,
            &lint_config,
            &overrides,
            &mut diagnostics,
            &mut operator_error,
        );
    }
    if operator_error {
        return EXIT_OPERATOR_ERROR;
    }

    let any_above_threshold = diagnostics
        .iter()
        .any(|(_, d)| SeverityLevel::from_diag(d) > args.max_severity);

    let use_color = resolve_color_from_env(args.color);
    render(args.format, &diagnostics, &root, args.quiet, use_color);

    if any_above_threshold {
        EXIT_LINT_FAILED
    } else {
        EXIT_OK
    }
}

/// Make the explicit `raven lint` command opt into the native linter.
///
/// This changes only the base `[linting].enabled` value. In particular, it
/// leaves per-file override switches and rule severities untouched so an
/// exclusion can still disable a matching file and an individual rule can
/// still be set to `off`. The normalization happens on the merged JSON before
/// both the typed base config and the override base section are derived;
/// otherwise a matching override would reparse the original disabled value.
fn force_cli_linting_enabled(settings: &mut serde_json::Value) {
    let Some(root) = settings.as_object_mut() else {
        *settings = json!({ "linting": { "enabled": true } });
        return;
    };
    let linting = root.entry("linting").or_insert_with(|| json!({}));
    let Some(linting) = linting.as_object_mut() else {
        *linting = json!({ "enabled": true });
        return;
    };
    linting.insert("enabled".to_string(), json!(true));
}

fn walk(
    path: &Path,
    root: &Path,
    base_section: &serde_json::Value,
    base_lint: &crate::linting::LintConfig,
    overrides: &[crate::config_file::CompiledLintOverride],
    out: &mut Vec<(PathBuf, Diagnostic)>,
    operator_error: &mut bool,
) {
    if path.is_file() {
        let chunk = is_chunk_file(path);
        if !chunk && !is_r_file(path) {
            return;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        if crate::config_file::is_skipped_by_overrides(base_section, overrides, rel) {
            return;
        }
        // `Url::from_file_path` requires an absolute path. The CLI is
        // commonly invoked with `raven lint .`, which produces relative
        // entries like `R/foo.R` from the directory walk — without
        // canonicalization the URL build falls back to `file:///` and
        // `resolve_lint_for_document`'s `strip_prefix(root)` check
        // silently drops every per-file `[[linting.overrides]]` patch.
        // Canonicalize against `root` to preserve file identity.
        let abs_path = absolute_path(root, path);
        let uri = tower_lsp::lsp_types::Url::from_file_path(&abs_path)
            .unwrap_or_else(|_| tower_lsp::lsp_types::Url::parse("file:///").unwrap());
        let effective =
            crate::config_file::resolve_lint_for_document(base_lint, base_section, overrides, &uri);
        // Decode through the shared BOM-aware seam so `raven lint` reads files
        // identically to the workspace scan and `raven check` (UTF-8 BOM
        // stripped, UTF-16 BOM decoded).
        let text = match crate::state::read_source(path) {
            Ok(t) => t,
            Err(crate::state::SourceReadError::Io(e)) => {
                eprintln!("raven lint: cannot read {}: {e}", path.display());
                *operator_error = true;
                return;
            }
            // A mis-encoded file (typically Latin-1 / Windows-1252 saved without
            // a BOM) is a property of the user's code, like a syntax error — so
            // report it as an ERROR finding (exit 1) instead of aborting the run
            // as an operator error (exit 2). This brings lint to parity with
            // `raven check`, replacing the cryptic "stream did not contain valid
            // UTF-8" abort with the actionable `encoding_diagnostic` message.
            Err(crate::state::SourceReadError::InvalidEncoding { offset, byte }) => {
                out.push((path.to_path_buf(), encoding_diagnostic(offset, byte)));
                return;
            }
        };
        // For chunk files (.Rmd/.Rmarkdown/.qmd), this masks the document to
        // its R chunk bodies before parsing and linting: all non-R-body lines
        // (prose, YAML, fences, non-R chunk bodies) become empty strings while
        // every line keeps its original index, so lint findings carry document
        // coordinates directly. Routed through the shared `analysis_text_for_kind`
        // chokepoint so `raven lint` derives its R-analysis view identically to
        // the LSP and `raven check`. Classify via the canonical
        // `classify_chunk_document` (not a local re-derivation of the `chunk`
        // guard above) so this site can never drift from the project-wide
        // `.Rmd`/`.Rmarkdown`/`.qmd` classifier.
        let chunk_kind = crate::chunks::classify_chunk_document(&path.to_string_lossy());
        let effective_text = crate::cross_file::analysis_text_for_kind(chunk_kind, &text);
        // The trailing-blank-lines rule describes the FILE's shape, but lints
        // run on the masked analysis text, where the closing fence / prose
        // after the last chunk is blanked — it would read as trailing blank
        // lines at EOF on virtually every Rmd/Quarto document. The raw file's
        // shape is Markdown, not R, so the rule simply doesn't apply to chunk
        // documents. Mirrors the editor (`DiagnosticsSnapshot::build`).
        let mut effective = effective;
        if chunk_kind == crate::chunks::ChunkKind::Rmd {
            effective.trailing_blank_lines_severity = None;
        }
        // Use the same thread-local parser pool the LSP uses; avoids
        // per-file Parser construction.
        let parse_result =
            crate::parser_pool::with_parser(|p| p.parse(effective_text.as_ref(), None));
        let tree = match parse_result {
            Some(t) => t,
            None => {
                eprintln!("raven lint: parse failed for {}", path.display());
                *operator_error = true;
                return;
            }
        };
        // F2 Step 4: chunk-level suppression (`raven.ignore=…` chunk option,
        // `# raven: ignore-chunk`). Computed from the RAW text (the chunk header
        // carrying the option is blanked in `effective_text`) and applied to the
        // lint findings, mirroring the diagnostics pipeline. `run_lints` already
        // honors per-line `# nolint` / `# raven:` markers inside chunk bodies.
        let chunk_meta = if chunk_kind == crate::chunks::ChunkKind::Rmd {
            let mut m = crate::cross_file::types::CrossFileMetadata::default();
            crate::chunks::append_chunk_suppressions(&mut m, &text);
            Some(m)
        } else {
            None
        };
        for d in crate::linting::run_lints(effective_text.as_ref(), tree.root_node(), &effective) {
            if let Some(meta) = &chunk_meta
                && let Some(tower_lsp::lsp_types::NumberOrString::String(code)) = &d.code
                && crate::linting::range_or_file_suppresses(meta, d.range.start.line, code)
            {
                continue;
            }
            out.push((path.to_path_buf(), d));
        }
    } else if path.is_dir() {
        let entries = match std::fs::read_dir(path) {
            Ok(it) => it,
            Err(e) => {
                eprintln!("raven lint: cannot read dir {}: {e}", path.display());
                *operator_error = true;
                return;
            }
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_symlink() {
                continue;
            }
            walk(
                &p,
                root,
                base_section,
                base_lint,
                overrides,
                out,
                operator_error,
            );
        }
    } else {
        eprintln!("raven lint: path does not exist: {}", path.display());
        *operator_error = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_paths() {
        let args = parse_args(Vec::<String>::new().into_iter()).unwrap();
        assert_eq!(args.paths, vec![PathBuf::from(".")]);
        assert_eq!(args.format, OutputFormat::Text);
        assert_eq!(args.max_severity, SeverityLevel::Info);
        assert_eq!(args.color, ColorChoice::Auto);
    }

    #[test]
    fn parse_color_and_no_color_alias() {
        let never = parse_args(["--color", "never"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(never.color, ColorChoice::Never);
        let alias = parse_args(["--no-color"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(alias.color, ColorChoice::Never);
        // Last-one-wins on conflict: `--no-color --color always` ⇒ always.
        let conflict = parse_args(
            ["--no-color", "--color", "always"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();
        assert_eq!(conflict.color, ColorChoice::Always);
        assert!(parse_args(["--color", "bogus"].iter().map(|s| s.to_string())).is_err());
    }

    #[test]
    fn parse_explicit_paths() {
        let args = parse_args(["R/", "scripts/foo.R"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(
            args.paths,
            vec![PathBuf::from("R/"), PathBuf::from("scripts/foo.R")]
        );
    }

    #[test]
    fn parse_format_json() {
        let args = parse_args(["--format", "json"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(args.format, OutputFormat::Json);
    }

    #[test]
    fn parse_max_severity_warning() {
        let args = parse_args(["--max-severity", "warning"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(args.max_severity, SeverityLevel::Warning);
    }

    #[test]
    fn unknown_flag_errors() {
        assert!(parse_args(["--bogus"].iter().map(|s| s.to_string())).is_err());
    }

    /// Args that route `resolve_lint_config` through the discover-and-load
    /// branch (no `--no-config`, no explicit `--config`).
    fn discovery_args() -> LintArgs {
        LintArgs {
            paths: vec![PathBuf::from(".")],
            config_path: None,
            no_config: false,
            format: OutputFormat::Text,
            max_severity: SeverityLevel::Info,
            quiet: true,
            color: ColorChoice::Never,
        }
    }

    #[test]
    fn resolve_lint_config_honors_discovered_lintr() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults()\n",
        )
        .unwrap();
        let (root, settings, lintr_discovered) =
            resolve_lint_config(tmp.path(), &discovery_args()).unwrap();
        assert_eq!(root, tmp.path().to_path_buf());
        assert!(
            settings.is_some(),
            "a discovered .lintr yields project settings"
        );
        assert!(
            lintr_discovered,
            "a discovered .lintr must set lintr_discovered so Auto resolution opts in"
        );
    }

    #[test]
    fn resolve_lint_config_reads_column_zero_lintr_file() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(\n    line_length_linter(120),\n    trailing_whitespace_linter = NULL\n)\n",
        )
        .unwrap();
        let (_root, settings, lintr_discovered) =
            resolve_lint_config(tmp.path(), &discovery_args()).unwrap();
        let settings = settings.expect("a discovered .lintr yields project settings");
        assert!(lintr_discovered, "a configured .lintr opts in");
        assert_eq!(settings["linting"]["lineLength"], serde_json::json!(120));
        assert_eq!(
            settings["linting"]["trailingWhitespaceSeverity"],
            serde_json::json!("off")
        );
    }

    #[test]
    fn resolve_lint_config_ignores_literal_home_lintr_by_default() {
        use std::fs;
        use tempfile::TempDir;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            home.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let discovery_options =
            crate::config_file::DiscoveryOptions::default().with_home_dir(home.path());

        let (root, settings, lintr_discovered) =
            resolve_lint_config_with_options(&workspace, &discovery_args(), &discovery_options)
                .unwrap();

        assert_eq!(root, workspace);
        assert!(
            settings.is_none(),
            "CLI auto-discovery should ignore literal home .lintr by default"
        );
        assert!(!lintr_discovered);
    }

    #[test]
    fn resolve_lint_config_honors_discovered_raven_toml() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("raven.toml"), "[linting]\nenabled = true\n").unwrap();
        let (root, settings, lintr_discovered) =
            resolve_lint_config(tmp.path(), &discovery_args()).unwrap();
        assert_eq!(root, tmp.path().to_path_buf());
        assert!(
            settings.is_some(),
            "a discovered raven.toml yields project settings"
        );
        assert!(
            !lintr_discovered,
            "raven.toml is not a .lintr, so lintr_discovered must stay false"
        );
    }

    #[test]
    fn resolve_lint_config_none_when_no_config_present() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let (root, settings, lintr_discovered) =
            resolve_lint_config(tmp.path(), &discovery_args()).unwrap();
        assert_eq!(root, tmp.path().to_path_buf());
        assert!(settings.is_none());
        assert!(!lintr_discovered);
    }

    #[test]
    fn resolve_lint_config_errors_on_malformed_raven_toml() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("raven.toml"), "not valid = = toml [[[\n").unwrap();
        assert_eq!(
            resolve_lint_config(tmp.path(), &discovery_args()),
            Err(EXIT_OPERATOR_ERROR)
        );
    }

    #[test]
    fn resolve_lint_config_normalizes_explicit_config_root() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::create_dir(tmp.path().join("pkg")).unwrap();
        fs::write(
            tmp.path().join("pkg/raven.toml"),
            "[linting]\nenabled = true\n",
        )
        .unwrap();

        // An absolute --config that routes through `sub/..`: the `..` must not
        // survive into `root`, or the purely lexical `strip_prefix(root)` in
        // `resolve_lint_for_document` would drop every override for a file given
        // by its (normalized) absolute path under the pkg root.
        let dotted = tmp
            .path()
            .join("sub")
            .join("..")
            .join("pkg")
            .join("raven.toml");
        let mut args = discovery_args();
        args.config_path = Some(dotted);

        let (root, _settings, lintr_discovered) = resolve_lint_config(tmp.path(), &args).unwrap();
        assert!(!lintr_discovered);
        assert!(
            !root.components().any(|c| matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )),
            "root still carries . / .. components: {root:?}"
        );
        assert!(
            root.ends_with("pkg"),
            "expected the pkg dir as root, got {root:?}"
        );
    }

    #[test]
    fn resolve_lint_config_honors_explicit_lintr() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let lintr = tmp.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let mut args = discovery_args();
        args.config_path = Some(lintr);

        let (root, settings, lintr_discovered) = resolve_lint_config(tmp.path(), &args).unwrap();

        assert_eq!(root, tmp.path().to_path_buf());
        assert_eq!(settings.unwrap()["linting"]["lineLength"], json!(120));
        assert!(lintr_discovered);
    }

    #[test]
    fn resolve_lint_config_explicit_home_lintr_keeps_cwd_as_root() {
        use std::fs;
        use tempfile::TempDir;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let lintr = home.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let mut args = discovery_args();
        args.config_path = Some(lintr);
        let discovery_options =
            crate::config_file::DiscoveryOptions::default().with_home_dir(home.path());

        let (root, settings, lintr_discovered) =
            resolve_lint_config_with_options(&workspace, &args, &discovery_options).unwrap();

        assert_eq!(
            root, workspace,
            "explicit ~/.lintr must not make $HOME the lint root"
        );
        assert_eq!(settings.unwrap()["linting"]["lineLength"], json!(120));
        assert!(lintr_discovered);
    }

    #[test]
    fn resolve_lint_config_explicit_non_home_lintr_uses_config_parent_as_root() {
        use std::fs;
        use tempfile::TempDir;

        let cwd = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let lintr = project.path().join(".lintr");
        fs::write(
            &lintr,
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let mut args = discovery_args();
        args.config_path = Some(lintr);

        let (root, settings, lintr_discovered) = resolve_lint_config(cwd.path(), &args).unwrap();

        assert_eq!(root, project.path().to_path_buf());
        assert_eq!(settings.unwrap()["linting"]["lineLength"], json!(120));
        assert!(lintr_discovered);
    }

    #[test]
    fn resolve_lint_config_relative_explicit_lintr_loads_from_cwd() {
        use std::fs;
        use tempfile::TempDir;

        let cwd = TempDir::new().unwrap();
        let config_dir = cwd.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(120))\n",
        )
        .unwrap();
        let mut args = discovery_args();
        args.config_path = Some(PathBuf::from("config").join(".lintr"));

        let (root, settings, lintr_discovered) = resolve_lint_config(cwd.path(), &args).unwrap();

        assert_eq!(root, config_dir);
        assert_eq!(settings.unwrap()["linting"]["lineLength"], json!(120));
        assert!(lintr_discovered);
    }

    #[test]
    fn walk_resolves_overrides_for_dotdot_paths_like_clean_paths() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("R")).unwrap();
        // A line well over 20 chars, so the base config flags it...
        fs::write(
            root.join("R").join("foo.R"),
            "x <- 'this line is intentionally way more than twenty characters wide'\n",
        )
        .unwrap();
        // ...but an `enabled = false` override for `R/*.R` skips the file
        // entirely. This exercises the `is_skipped_by_overrides` path, which
        // (unlike `resolve_lint_for_document`) does NOT round-trip through a URL
        // — so it sees the raw path and is the one that breaks on `..`.
        let settings = serde_json::json!({
            "linting": {
                "enabled": true,
                "lineLength": 20,
                "lineLengthSeverity": "warning",
                "overrides": [ { "files": ["R/*.R"], "enabled": false } ]
            }
        });
        let base_lint = crate::backend::parse_lint_config(&settings, false).unwrap();
        let base_section = settings.get("linting").cloned().unwrap();
        let overrides = crate::config_file::compile_lint_overrides(&settings, root);

        let run = |p: &Path| {
            let mut diags = Vec::new();
            let mut operator_error = false;
            walk(
                p,
                root,
                &base_section,
                &base_lint,
                &overrides,
                &mut diags,
                &mut operator_error,
            );
            assert!(!operator_error, "unexpected operator error for {p:?}");
            diags.len()
        };

        // Characterization guard: a file referenced via a `..`-laden absolute
        // path must resolve `[[linting.overrides]]` exactly as the clean path
        // does. This already holds because `Url::from_file_path` performs
        // RFC-3986 dot-segment removal, so `resolve_lint_for_document` (the
        // authoritative override application) sees a normalized path and the
        // `R/*.R` glob still matches. (`is_skipped_by_overrides` does see the
        // raw `R/../R/foo.R` and miss, but that's only a pre-parse skip
        // optimization — `run_lints` returns nothing for the disabled override
        // either way, so the diagnostics are identical.) Locks the behavior in
        // so a future change to the URI construction can't silently regress it.
        let clean = root.join("R").join("foo.R");
        let dotted = root.join("R").join("..").join("R").join("foo.R");
        assert_eq!(
            run(&dotted),
            run(&clean),
            "a `..`-laden path must resolve overrides the same as the clean path"
        );
    }

    #[test]
    fn end_to_end_finds_line_length_violation() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("raven.toml"),
            "[linting]\nenabled = true\nlineLength = 20\nlineLengthSeverity = \"warning\"\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("over.R"),
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        // Use --config + absolute path arguments instead of mutating CWD.
        // CWD is process-global; cargo runs tests in parallel by default, so
        // touching it from a test races with any other test that does the same.
        let args = LintArgs {
            paths: vec![tmp.path().to_path_buf()],
            config_path: Some(tmp.path().join("raven.toml")),
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Info,
            quiet: true,
            color: ColorChoice::Never,
        };
        // Redirect stdout to a buffer is non-trivial; instead just call run() and
        // assert the exit code. Stdout assertions live in the integration test
        // suite that runs the binary.
        let code = run(args);
        assert_eq!(code, EXIT_LINT_FAILED); // warning > info default
    }

    fn end_to_end_args(path: &Path) -> LintArgs {
        LintArgs {
            paths: vec![path.to_path_buf()],
            config_path: None,
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Off,
            quiet: true,
            color: ColorChoice::Never,
        }
    }

    #[test]
    fn end_to_end_lint_enables_defaults_without_config() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("assignment.R");
        fs::write(&file, "x = 1\n").unwrap();

        let args = end_to_end_args(&file);
        assert_eq!(
            run_with_cwd(args.clone(), tmp.path()),
            EXIT_LINT_FAILED,
            "an explicit lint command must run the built-in rules without a config"
        );

        let mut no_config = args;
        no_config.no_config = true;
        assert_eq!(
            run_with_cwd(no_config, tmp.path()),
            EXIT_LINT_FAILED,
            "--no-config must still run the built-in rules"
        );
    }

    #[test]
    fn end_to_end_lint_ignores_base_off_but_keeps_custom_rule_values() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("raven.toml");
        let file = tmp.path().join("over.R");
        fs::write(
            &config,
            "[linting]\nenabled = false\nlineLength = 20\nlineLengthSeverity = \"warning\"\n",
        )
        .unwrap();
        fs::write(
            &file,
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        let mut args = end_to_end_args(&file);
        args.config_path = Some(config);
        args.max_severity = SeverityLevel::Info;
        assert_eq!(run_with_cwd(args, tmp.path()), EXIT_LINT_FAILED);
    }

    #[test]
    fn end_to_end_matching_override_inherits_forced_enabled_base() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let tests = tmp.path().join("tests");
        fs::create_dir(&tests).unwrap();
        let config = tmp.path().join("raven.toml");
        let file = tests.join("over.R");
        fs::write(
            &config,
            "[linting]\nenabled = false\nlineLength = 1000\nlineLengthSeverity = \"warning\"\n\n[[linting.overrides]]\nfiles = [\"tests/*.R\"]\nlineLength = 20\n",
        )
        .unwrap();
        fs::write(
            &file,
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        let mut args = end_to_end_args(&file);
        args.config_path = Some(config);
        args.max_severity = SeverityLevel::Info;
        assert_eq!(run_with_cwd(args, tmp.path()), EXIT_LINT_FAILED);
    }

    #[test]
    fn end_to_end_override_off_still_skips_only_matching_files() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("raven.toml");
        let skipped = tmp.path().join("skip.R");
        let linted = tmp.path().join("keep.R");
        fs::write(
            &config,
            "[linting]\nenabled = false\nassignmentOperatorSeverity = \"warning\"\n\n[[linting.overrides]]\nfiles = [\"skip.R\"]\nenabled = false\n",
        )
        .unwrap();
        fs::write(&skipped, "x = 1\n").unwrap();
        fs::write(&linted, "x = 1\n").unwrap();

        let mut skipped_args = end_to_end_args(&skipped);
        skipped_args.config_path = Some(config.clone());
        skipped_args.max_severity = SeverityLevel::Info;
        assert_eq!(run_with_cwd(skipped_args, tmp.path()), EXIT_OK);

        let mut linted_args = end_to_end_args(&linted);
        linted_args.config_path = Some(config);
        linted_args.max_severity = SeverityLevel::Info;
        assert_eq!(run_with_cwd(linted_args, tmp.path()), EXIT_LINT_FAILED);
    }

    #[test]
    fn end_to_end_rule_severity_off_remains_silent() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("raven.toml");
        let file = tmp.path().join("assignment.R");
        fs::write(
            &config,
            "[linting]\nenabled = false\nassignmentOperatorSeverity = \"off\"\n",
        )
        .unwrap();
        fs::write(&file, "x = 1\n").unwrap();

        let mut args = end_to_end_args(&file);
        args.config_path = Some(config);
        assert_eq!(run_with_cwd(args, tmp.path()), EXIT_OK);
    }

    #[test]
    fn end_to_end_explicit_lintr_finds_line_length_violation() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(20))\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("over.R"),
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        let args = LintArgs {
            paths: vec![tmp.path().to_path_buf()],
            config_path: Some(tmp.path().join(".lintr")),
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Off,
            quiet: true,
            color: ColorChoice::Never,
        };

        assert_eq!(run(args), EXIT_LINT_FAILED);
    }

    #[test]
    fn end_to_end_explicit_home_lintr_lints_project_cwd_default_path() {
        use std::fs;
        use tempfile::TempDir;

        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            home.path().join(".lintr"),
            "linters: linters_with_defaults(line_length_linter(20))\n",
        )
        .unwrap();
        fs::write(
            workspace.join("over.R"),
            "x <- \"this line is intentionally way more than twenty characters wide\"\n",
        )
        .unwrap();

        let args = LintArgs {
            paths: vec![PathBuf::from(".")],
            config_path: Some(home.path().join(".lintr")),
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Off,
            quiet: true,
            color: ColorChoice::Never,
        };
        let discovery_options =
            crate::config_file::DiscoveryOptions::default().with_home_dir(home.path());

        assert_eq!(
            run_with_cwd_and_options(args, &workspace, &discovery_options),
            EXIT_LINT_FAILED
        );
    }

    /// Args that lint one file with `enabled` linting, sharing the boilerplate
    /// the encoding tests need (`walk` requires a parsed config + overrides).
    fn lint_one(
        root: &Path,
        file: &Path,
        settings: &serde_json::Value,
    ) -> (Vec<(PathBuf, Diagnostic)>, bool) {
        let base_lint = crate::backend::parse_lint_config(settings, false).unwrap();
        let base_section = settings.get("linting").cloned().unwrap();
        let overrides = crate::config_file::compile_lint_overrides(settings, root);
        let mut diags = Vec::new();
        let mut operator_error = false;
        walk(
            file,
            root,
            &base_section,
            &base_lint,
            &overrides,
            &mut diags,
            &mut operator_error,
        );
        (diags, operator_error)
    }

    #[test]
    fn walk_reports_non_utf8_as_finding_not_operator_error() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // A Latin-1 / Windows-1252 file: a bare 0xA0 (non-breaking space) with
        // no UTF-16 BOM. `raven check` reports this as an ERROR finding (exit 1),
        // not an operator error (exit 2); `raven lint` must match instead of
        // aborting with the cryptic "stream did not contain valid UTF-8".
        let mut bytes = b"x <- 1\n".to_vec();
        bytes.push(0xA0);
        bytes.push(b'\n');
        let file = root.join("latin1.R");
        fs::write(&file, bytes).unwrap();

        let settings = serde_json::json!({ "linting": { "enabled": true } });
        let (diags, operator_error) = lint_one(root, &file, &settings);

        assert!(
            !operator_error,
            "a mis-encoded file is a property of the code, not an operator error"
        );
        assert_eq!(
            diags.len(),
            1,
            "expected exactly one encoding finding, got {diags:?}"
        );
        let d = &diags[0].1;
        assert_eq!(
            d.severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
        );
        assert!(d.message.contains("not valid UTF-8"), "{}", d.message);
    }

    #[test]
    fn walk_strips_utf8_bom_before_measuring_line_length() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Line 1 is exactly 20 UTF-16 units (the clamp floor for `lineLength`)
        // *after* the BOM is stripped. With a leftover U+FEFF (len_utf16 == 1)
        // it would measure 21 and trip line_length; reading through the
        // BOM-aware seam strips it, so the file is clean. (Pre-migration
        // `read_to_string` keeps the BOM → this fails.)
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"x <- \"aaaaaaaaaaaaa\"\n"); // 20 chars after the BOM
        let file = root.join("bom.R");
        fs::write(&file, bytes).unwrap();

        let settings = serde_json::json!({
            "linting": { "enabled": true, "lineLength": 20, "lineLengthSeverity": "warning" }
        });
        let (diags, operator_error) = lint_one(root, &file, &settings);

        assert!(!operator_error);
        assert!(
            !diags
                .iter()
                .any(|(_, d)| d.message.contains("characters long")),
            "BOM must be stripped before line-length measurement; got {diags:?}"
        );
    }

    #[test]
    fn run_reports_non_utf8_file_as_finding_exit_1() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("raven.toml"), "[linting]\nenabled = true\n").unwrap();
        let mut bytes = b"x <- 1\n".to_vec();
        bytes.push(0xA0);
        bytes.push(b'\n');
        fs::write(tmp.path().join("latin1.R"), bytes).unwrap();

        let args = LintArgs {
            paths: vec![tmp.path().to_path_buf()],
            config_path: Some(tmp.path().join("raven.toml")),
            no_config: false,
            format: OutputFormat::Json,
            max_severity: SeverityLevel::Info,
            quiet: true,
            color: ColorChoice::Never,
        };
        // Before this migration a non-UTF-8 file set operator_error → exit 2.
        // Now it is an ERROR finding (parity with `raven check`) → exit 1.
        assert_eq!(run(args), EXIT_LINT_FAILED);
    }

    /// Settings that enable linting with the `assignment_operator` rule set to
    /// flag `=` as a warning (default style is `<-`; for tests we use WARNING
    /// so we can distinguish from no-finding).
    fn assignment_warn_settings() -> serde_json::Value {
        serde_json::json!({
            "linting": {
                "enabled": true,
                "assignmentOperatorSeverity": "warning"
            }
        })
    }

    /// A minimal .Rmd document with one R chunk containing `x=1` (assignment
    /// operator finding) on line 3 (0-based), with surrounding prose. The prose
    /// lines contain `y=2` which would be a finding if treated as R code.
    fn rmd_with_chunk_finding() -> &'static str {
        "---\ntitle: Test\n---\n\n```{r}\nx=1\n```\n\nProse y=2 here.\n"
    }

    #[test]
    fn lint_flags_finding_inside_rmd_chunk_at_document_coords() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let file = root.join("doc.Rmd");
        fs::write(&file, rmd_with_chunk_finding()).unwrap();

        let settings = assignment_warn_settings();
        let (diags, operator_error) = lint_one(root, &file, &settings);

        assert!(!operator_error, "unexpected operator error");
        // `x=1` is on line 5 (0-based) of the document (after "---", "title:
        // Test", "---", "", "```{r}"). run_lints reports 0-based lines, so
        // range.start.line == 5. There must be at least one finding here.
        let chunk_finding = diags.iter().find(|(p, d)| {
            p == &file
                && d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "assignment_operator".to_string(),
                    ))
        });
        assert!(
            chunk_finding.is_some(),
            "expected an assignment_operator finding; got {diags:?}"
        );
        let (_, d) = chunk_finding.unwrap();
        assert_eq!(
            d.range.start.line, 5,
            "finding must be at document line 5 (0-based); got {:?}",
            d.range.start.line
        );
        // Prose line 8 (`Prose y=2 here.`) must NOT produce an
        // assignment_operator finding — it is masked out as non-R.
        assert!(
            !diags.iter().any(|(_, d2)| {
                d2.range.start.line == 8
                    && d2.code
                        == Some(tower_lsp::lsp_types::NumberOrString::String(
                            "assignment_operator".to_string(),
                        ))
            }),
            "prose line must not produce an assignment_operator finding"
        );
    }

    #[test]
    fn lint_chunk_option_raven_ignore_suppresses_lint_finding() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // `x=1` (assignment_operator finding) inside a `raven.ignore=TRUE`
        // chunk must be suppressed for `raven lint`, mirroring the diagnostics
        // pipeline (F2 Step 4).
        let content = "---\ntitle: T\n---\n\n```{r, raven.ignore=TRUE}\nx=1\n```\n";
        let file = root.join("doc.Rmd");
        fs::write(&file, content).unwrap();

        let settings = assignment_warn_settings();
        let (diags, operator_error) = lint_one(root, &file, &settings);
        assert!(!operator_error, "unexpected operator error");
        assert!(
            !diags.iter().any(|(_, d)| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "assignment_operator".to_string()
                ))),
            "raven.ignore=TRUE chunk must suppress the lint finding; got {diags:?}"
        );
    }

    #[test]
    fn lint_in_chunk_ignore_chunk_directive_suppresses_lint_finding() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let content = "---\ntitle: T\n---\n\n```{r}\n# raven: ignore-chunk\nx=1\n```\n";
        let file = root.join("doc.Rmd");
        fs::write(&file, content).unwrap();

        let settings = assignment_warn_settings();
        let (diags, operator_error) = lint_one(root, &file, &settings);
        assert!(!operator_error, "unexpected operator error");
        assert!(
            !diags.iter().any(|(_, d)| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "assignment_operator".to_string()
                ))),
            "# raven: ignore-chunk must suppress the lint finding; got {diags:?}"
        );
    }

    #[test]
    fn lint_ignores_rmd_prose() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Prose-only .Rmd: `x=1` in prose would trigger assignment_operator if
        // treated as R, but masking must blank it. No R chunks at all.
        let content = "---\ntitle: Test\n---\n\nThis is prose. x=1 here.";
        let file = root.join("prose_only.Rmd");
        fs::write(&file, content).unwrap();

        let settings = assignment_warn_settings();
        let (diags, operator_error) = lint_one(root, &file, &settings);

        assert!(!operator_error, "unexpected operator error");
        // No assignment_operator finding must appear — prose is masked.
        assert!(
            !diags.iter().any(|(_, d)| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "assignment_operator".to_string()
                ))),
            "prose-only Rmd must produce no assignment_operator finding; got {diags:?}"
        );
    }

    #[test]
    fn lint_nolint_inside_chunk_respected() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Line 5 (0-based) = `x=1 # nolint`  → assignment_operator suppressed
        // Line 6 (0-based) = `y=2`            → assignment_operator flagged (non-vacuous)
        let content = "---\ntitle: Test\n---\n\n```{r}\nx=1 # nolint\ny=2\n```";
        let file = root.join("nolint.Rmd");
        fs::write(&file, content).unwrap();

        let settings = assignment_warn_settings();
        let (diags, operator_error) = lint_one(root, &file, &settings);

        assert!(!operator_error, "unexpected operator error");
        // `x=1 # nolint` on line 5 must be suppressed: no assignment_operator
        // finding at line 5.
        assert!(
            !diags.iter().any(|(_, d)| {
                d.range.start.line == 5
                    && d.code
                        == Some(tower_lsp::lsp_types::NumberOrString::String(
                            "assignment_operator".to_string(),
                        ))
            }),
            "assignment_operator on line 5 must be suppressed by # nolint; got {diags:?}"
        );
        // `y=2` on line 6 must produce an assignment_operator finding (non-vacuous).
        assert!(
            diags.iter().any(|(_, d)| {
                d.range.start.line == 6
                    && d.code
                        == Some(tower_lsp::lsp_types::NumberOrString::String(
                            "assignment_operator".to_string(),
                        ))
            }),
            "assignment_operator on line 6 (y=2) must not be suppressed; got {diags:?}"
        );
    }

    #[test]
    fn lint_rmd_exempt_from_trailing_blank_lines() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // The closing fence and prose after the last chunk are blanked in the
        // masked analysis text, so the file-shape rule would otherwise read
        // them as trailing blank lines at EOF on virtually every Rmd. The rule
        // is disabled for chunk documents (the raw file's shape is Markdown,
        // not R) — but stays active for plain .R files.
        let rmd = root.join("doc.Rmd");
        fs::write(&rmd, "```{r}\nx <- 1\n```\n\nClosing prose.\n").unwrap();
        let r_file = root.join("plain.R");
        fs::write(&r_file, "x <- 1\n\n\n").unwrap();

        let settings = serde_json::json!({
            "linting": { "enabled": true, "trailingBlankLinesSeverity": "warning" }
        });
        let trailing = |diags: &[(PathBuf, Diagnostic)]| {
            diags.iter().any(|(_, d)| {
                d.code
                    == Some(tower_lsp::lsp_types::NumberOrString::String(
                        "trailing_blank_lines".to_string(),
                    ))
            })
        };

        let (rmd_diags, err) = lint_one(root, &rmd, &settings);
        assert!(!err, "unexpected operator error");
        assert!(
            !trailing(&rmd_diags),
            "masked Rmd must not produce trailing_blank_lines findings; got {rmd_diags:?}"
        );

        let (r_diags, err) = lint_one(root, &r_file, &settings);
        assert!(!err, "unexpected operator error");
        assert!(
            trailing(&r_diags),
            "plain .R must still be flagged (non-vacuous); got {r_diags:?}"
        );
    }

    #[test]
    fn lint_walk_includes_chunk_files() {
        use std::fs;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // A directory with one .Rmd, one .Rmarkdown, one .qmd, and one plain
        // .R; all have a triggering pattern so all should produce findings
        // (.Rmarkdown/.qmd share the .Rmd dispatch path via is_chunk_file —
        // pin them end-to-end too).
        let rmd = root.join("doc.Rmd");
        let rmarkdown = root.join("notebook.Rmarkdown");
        let qmd = root.join("paper.qmd");
        let r_file = root.join("plain.R");
        fs::write(&rmd, rmd_with_chunk_finding()).unwrap();
        fs::write(&rmarkdown, rmd_with_chunk_finding()).unwrap();
        fs::write(&qmd, rmd_with_chunk_finding()).unwrap();
        fs::write(&r_file, "x=1\n").unwrap();

        let settings = assignment_warn_settings();
        // Walk the directory, not individual files (`lint_one` hands its
        // `file` argument straight to `walk`, which dispatches dirs too).
        let (diags, operator_error) = lint_one(root, root, &settings);

        assert!(!operator_error, "unexpected operator error");
        // All four files must contribute findings.
        let rmd_findings: Vec<_> = diags.iter().filter(|(p, _)| p == &rmd).collect();
        let rmarkdown_findings: Vec<_> = diags.iter().filter(|(p, _)| p == &rmarkdown).collect();
        let qmd_findings: Vec<_> = diags.iter().filter(|(p, _)| p == &qmd).collect();
        let r_findings: Vec<_> = diags.iter().filter(|(p, _)| p == &r_file).collect();
        assert!(
            !rmd_findings.is_empty(),
            "directory walk must include .Rmd findings; got {diags:?}"
        );
        assert!(
            !rmarkdown_findings.is_empty(),
            "directory walk must include .Rmarkdown findings; got {diags:?}"
        );
        assert!(
            !qmd_findings.is_empty(),
            "directory walk must include .qmd findings; got {diags:?}"
        );
        assert!(
            !r_findings.is_empty(),
            "directory walk must include .R findings; got {diags:?}"
        );
    }
}
