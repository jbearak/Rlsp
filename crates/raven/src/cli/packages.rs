//! `raven packages {fetch,freeze,update,build-shipped-db}` — package-database commands.
//!
//! `fetch` (this work) and `freeze` (Task 5.3) are the two producers of a repo's
//! Tier 2 `.raven/packages.json`: `freeze` captures it from a local R install,
//! `fetch` from CRAN/Bioc r-universe (R-free, additive merge — existing rows win).
//! `build-shipped-db` is the maintainer-only Tier 3 builder. It merges, **append-only
//! and version-monotonic**, three sources into `names.db`: the prior DB (the seed),
//! an authoritative reference-R capture of the build machine's installed library,
//! and CRAN + Bioc r-universe metadata. The shipped binary never fetches from the
//! network; a build job (`scripts/build-names-db.sh`) supplies the r-universe data
//! with `curl` — one bulk `/api/dbdump` BSON file per universe (`--runiverse-cran`
//! / `--runiverse-bioc` accept that file, or a legacy per-package JSON directory).
//! `--runiverse-{cran,bioc}-min` carry the `/api/ls` floor that gates coverage.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs::OpenOptions, io::Write};

use crate::cli::shared::absolute_path;
use crate::package_db::binary_db::{ShippedDb, ShippedDbProvenance, write_shipped_db};
use crate::package_db::json_db::{
    REPO_DB_SCHEMA_VERSION, RepoDb, RepoDbProvenance, read_repo_db_file, write_repo_db_file,
};
use crate::package_db::merge::merge_append_only;
use crate::package_db::model::PackageRecord;
use crate::package_db::renv_lock::read_renv_lock_package_names;
use crate::package_db::runiverse::ingest_runiverse_path;
use crate::r_subprocess::is_valid_package_name;

const DEFAULT_NAMES_DB_RELEASE_BASE: &str =
    "https://github.com/jbearak/raven/releases/download/names-db";

#[derive(Debug)]
pub struct UpdateArgs {
    pub base_url: String,
    pub dest_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub struct InstalledSidecars {
    pub names_db_path: PathBuf,
    pub names_db_provenance: ShippedDbProvenance,
}

/// Whether `s` is a real `YYYY-MM-DD` calendar date. Beyond the `dddd-dd-dd`
/// shape this rejects impossible dates (e.g. `2026-02-31`) by validating the
/// month and the day against that month's length, leap years included. It gates
/// the optional `update <date>` positional so a stray token can't be mistaken
/// for a date; a valid but unpublished date surfaces later as a 404 from the
/// Release.
fn is_yyyy_mm_dd(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        s[0..4].parse::<u32>(),
        s[5..7].parse::<u32>(),
        s[8..10].parse::<u32>(),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days_in_month).contains(&day)
}

pub fn parse_update_args(mut argv: impl Iterator<Item = String>) -> Result<UpdateArgs, String> {
    let mut base_url: Option<String> = None;
    let mut dest_dir = None;
    let mut date: Option<String> = None;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--base-url" => base_url = Some(argv.next().ok_or("--base-url needs a URL")?),
            "--dest-dir" => {
                dest_dir = Some(PathBuf::from(argv.next().ok_or("--dest-dir needs a path")?))
            }
            "--help" => return Err("HELP".into()),
            s if s.starts_with('-') => return Err(format!("unknown flag: {s}")),
            s if date.is_some() => return Err(format!("unexpected extra argument: {s}")),
            s if !is_yyyy_mm_dd(s) => {
                return Err(format!("expected a release date as YYYY-MM-DD, got: {s}"));
            }
            s => date = Some(s.to_string()),
        }
    }
    // `--base-url` already encodes the full tag path, so combining it with a date
    // (which also selects the tag) is ambiguous. The date only rewrites the tag
    // suffix on the default GitHub base.
    let base_url = match (base_url, date) {
        (Some(_), Some(_)) => {
            return Err("pass either a YYYY-MM-DD release date or --base-url, not both".into());
        }
        (Some(b), None) => b,
        (None, Some(d)) => format!("{DEFAULT_NAMES_DB_RELEASE_BASE}-{d}"),
        (None, None) => DEFAULT_NAMES_DB_RELEASE_BASE.to_string(),
    };
    Ok(UpdateArgs { base_url, dest_dir })
}

#[derive(Debug)]
pub struct BuildShippedDbArgs {
    pub capture_reference: bool,
    pub runiverse_cran: Option<PathBuf>,
    pub runiverse_bioc: Option<PathBuf>,
    /// Minimum distinct packages the CRAN source must yield, or the build aborts
    /// (the authoritative coverage gate — the build script passes the `/api/ls`
    /// count less a small tolerance). `None` skips the check.
    pub runiverse_cran_min: Option<usize>,
    pub runiverse_bioc_min: Option<usize>,
    pub fresh: bool,
    pub seed: Option<PathBuf>,
    pub output: PathBuf,
    pub snapshot_date: String,
    pub source: String,
}

pub fn parse_build_shipped_db_args(
    mut argv: impl Iterator<Item = String>,
) -> Result<BuildShippedDbArgs, String> {
    let mut runiverse_cran = None;
    let mut runiverse_bioc = None;
    let mut runiverse_cran_min = None;
    let mut runiverse_bioc_min = None;
    let mut fresh = false;
    let mut seed = None;
    let mut output = None;
    let mut snapshot_date = String::new();
    let mut source = "reference-R ∪ r-universe".to_string();
    let parse_min = |argv: &mut dyn Iterator<Item = String>, flag: &str| {
        let n = argv
            .next()
            .ok_or_else(|| format!("{flag} needs a count"))?
            .parse::<usize>()
            .map_err(|e| format!("{flag} needs a non-negative integer: {e}"))?;
        // A floor of 0 is satisfied by any distinct count, silently disabling the
        // coverage gate — refuse it so a direct CLI invocation can't bypass the
        // truncation guard the shell wrapper already enforces.
        if n == 0 {
            return Err(format!(
                "{flag} must be a positive integer (a floor of 0 disables the coverage gate)"
            ));
        }
        Ok(n)
    };
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--runiverse-cran" => {
                runiverse_cran = Some(PathBuf::from(
                    argv.next().ok_or("--runiverse-cran needs a path")?,
                ))
            }
            "--runiverse-bioc" => {
                runiverse_bioc = Some(PathBuf::from(
                    argv.next().ok_or("--runiverse-bioc needs a path")?,
                ))
            }
            "--runiverse-cran-min" => {
                runiverse_cran_min = Some(parse_min(&mut argv, "--runiverse-cran-min")?)
            }
            "--runiverse-bioc-min" => {
                runiverse_bioc_min = Some(parse_min(&mut argv, "--runiverse-bioc-min")?)
            }
            "--fresh" | "--no-seed" => fresh = true,
            "--seed" => seed = Some(PathBuf::from(argv.next().ok_or("--seed needs a path")?)),
            "--output" => output = Some(PathBuf::from(argv.next().ok_or("--output needs a path")?)),
            "--snapshot-date" => {
                snapshot_date = argv.next().ok_or("--snapshot-date needs a value")?
            }
            "--source" => source = argv.next().ok_or("--source needs a value")?,
            "--help" => return Err("HELP".into()),
            s => return Err(format!("unknown flag: {s}")),
        }
    }
    if runiverse_cran_min.is_some() && runiverse_cran.is_none() {
        return Err("--runiverse-cran-min requires --runiverse-cran".into());
    }
    if runiverse_bioc_min.is_some() && runiverse_bioc.is_none() {
        return Err("--runiverse-bioc-min requires --runiverse-bioc".into());
    }
    Ok(BuildShippedDbArgs {
        capture_reference: true,
        runiverse_cran,
        runiverse_bioc,
        runiverse_cran_min,
        runiverse_bioc_min,
        fresh,
        seed,
        output: output.ok_or("--output is required")?,
        snapshot_date,
        source,
    })
}

#[derive(Debug)]
pub struct ValidateShippedDbArgs {
    pub path: PathBuf,
}

pub fn parse_validate_shipped_db_args(
    mut argv: impl Iterator<Item = String>,
) -> Result<ValidateShippedDbArgs, String> {
    let Some(path) = argv.next() else {
        return Err("validate-shipped-db needs a names.db path".into());
    };
    if path == "--help" {
        return Err("HELP".into());
    }
    if let Some(extra) = argv.next() {
        return Err(format!("unexpected extra argument: {extra}"));
    }
    Ok(ValidateShippedDbArgs {
        path: PathBuf::from(path),
    })
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FreezeScope {
    Used,
    All,
}

pub struct FreezeArgs {
    pub scope: FreezeScope,
    pub output: PathBuf,
    pub workspace: Option<PathBuf>,
}

pub fn parse_freeze_args(mut argv: impl Iterator<Item = String>) -> Result<FreezeArgs, String> {
    let mut scope = FreezeScope::Used;
    let mut output = PathBuf::from(".raven/packages.json");
    let mut workspace = None;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--used" => scope = FreezeScope::Used,
            "--installed" | "--all" => scope = FreezeScope::All,
            "--output" => output = PathBuf::from(argv.next().ok_or("--output needs a path")?),
            "--workspace" => {
                workspace = Some(PathBuf::from(
                    argv.next().ok_or("--workspace needs a path")?,
                ))
            }
            "--help" => return Err("HELP".into()),
            s => return Err(format!("unknown flag: {s}")),
        }
    }
    Ok(FreezeArgs {
        scope,
        output,
        workspace,
    })
}

#[derive(Debug)]
pub struct FetchArgs {
    pub missing_only: bool,
    pub fail_on_missing: bool,
    pub output: PathBuf,
    pub workspace: Option<PathBuf>,
    pub base_urls: Vec<String>,
}

pub fn parse_fetch_args(mut argv: impl Iterator<Item = String>) -> Result<FetchArgs, String> {
    let mut missing_only = false;
    let mut fail_on_missing = false;
    let mut output = PathBuf::from(".raven/packages.json");
    let mut workspace = None;
    let mut base_urls: Option<Vec<String>> = None;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--missing-only" => missing_only = true,
            "--fail-on-missing" => fail_on_missing = true,
            "--output" => output = PathBuf::from(argv.next().ok_or("--output needs a path")?),
            "--workspace" => {
                workspace = Some(PathBuf::from(
                    argv.next().ok_or("--workspace needs a path")?,
                ))
            }
            "--base-urls" => {
                let raw = argv.next().ok_or("--base-urls needs a value")?;
                let parsed: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if parsed.is_empty() {
                    return Err("--base-urls needs at least one non-empty URL".into());
                }
                base_urls = Some(parsed);
            }
            "--help" => return Err("HELP".into()),
            s => return Err(format!("unknown flag: {s}")),
        }
    }
    Ok(FetchArgs {
        missing_only,
        fail_on_missing,
        output,
        workspace,
        base_urls: base_urls.unwrap_or_else(|| {
            vec![
                "https://cran.r-universe.dev".into(),
                "https://bioc.r-universe.dev".into(),
            ]
        }),
    })
}

/// One warning line per fetched record whose version differs from the renv.lock pin.
fn renv_skew_warnings(
    fetched: &[PackageRecord],
    pinned: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for rec in fetched {
        if let Some(want) = pinned.get(&rec.name)
            && !want.is_empty()
            && want != &rec.version
        {
            out.push(format!(
                "{}: fetched {} (latest); renv.lock pins {}. Export names usually match \
                     across versions; install it and use `freeze` / `--missing-only` for a \
                     version-exact capture.",
                rec.name, rec.version, want
            ));
        }
    }
    out
}

fn write_repo_db_file_atomic(path: &std::path::Path, db: &RepoDb) -> Result<(), String> {
    use crate::package_db::json_db::write_repo_db_string;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("packages.json");
    let mut text = write_repo_db_string(db);
    text.push('\n');
    let tmp = write_unique_temp(dir, file_name, text.into_bytes())?;
    if let Err(e) = replace_with_tmp(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Fetch package metadata from r-universe, following transitive dependencies.
pub async fn run_fetch(args: FetchArgs) -> Result<(), String> {
    use crate::package_db::renv_lock::read_renv_lock_package_versions;
    use std::collections::HashSet;
    use std::time::{SystemTime, UNIX_EPOCH};

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = match &args.workspace {
        Some(p) => absolute_path(&cwd, p),
        None => cwd,
    };
    let out = absolute_path(&root, &args.output);

    // Step 2: read existing
    let existing_records = match read_repo_db_file(&out) {
        Ok(db) => db.packages,
        Err(crate::package_db::json_db::RepoDbError::Absent) => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    let existing_names: HashSet<String> = existing_records.iter().map(|r| r.name.clone()).collect();

    // Step 3: base packages
    let base = embedded_base_packages();

    // Step 4: tier1 lib for --missing-only
    let lib_outcome = if args.missing_only {
        Some(
            crate::package_library::build_package_library_tier1_only(None, &[], Some(root.clone()))
                .await,
        )
    } else {
        None
    };
    let is_installed = |name: &str| -> bool {
        args.missing_only
            && lib_outcome
                .as_ref()
                .is_some_and(|o| o.library.package_exists(name))
    };

    // Step 5: used packages (non-base, deduped)
    let mut used_nonbase: Vec<String> = collect_used_package_names(&root)?
        .into_iter()
        .filter(|name| !base.contains(name))
        .collect();
    used_nonbase.sort();
    used_nonbase.dedup();

    // Step 6: initial fetch set
    let to_fetch: Vec<String> = used_nonbase
        .iter()
        .filter(|n| !existing_names.contains(*n) && !is_installed(n))
        .cloned()
        .collect();

    // Step 7: inform
    if !to_fetch.is_empty() {
        eprintln!(
            "Fetching {} package(s) from r-universe: {}",
            to_fetch.len(),
            to_fetch.join(", ")
        );
    }

    // Step 8: BFS waves
    let mut seen: HashSet<String> = to_fetch.iter().cloned().collect();
    let mut found_names: HashSet<String> = HashSet::new();
    let mut fetched_records: Vec<PackageRecord> = Vec::new();
    let mut found_count: usize = 0;
    let mut transport_count: usize = 0;
    let mut sample_transport: Option<String> = None;
    let mut queue = to_fetch;

    while !queue.is_empty() {
        let results = fetch_packages_wave(std::mem::take(&mut queue), &args.base_urls).await;
        let mut next_queue: Vec<String> = Vec::new();
        for (name, outcome) in results {
            match outcome {
                PackageFetchOutcome::Found(rec) => {
                    found_count += 1;
                    found_names.insert(name);
                    for dep in &rec.depends {
                        if dep != "R"
                            && !base.contains(dep)
                            && !existing_names.contains(dep)
                            && !is_installed(dep)
                            && seen.insert(dep.clone())
                        {
                            next_queue.push(dep.clone());
                        }
                    }
                    fetched_records.push(rec);
                }
                PackageFetchOutcome::NotFound => {}
                PackageFetchOutcome::Transport(msg) => {
                    transport_count += 1;
                    if sample_transport.is_none() {
                        sample_transport = Some(msg);
                    }
                }
            }
        }
        queue = next_queue;
    }

    // Step 9: hard error when infrastructure failure prevented any successful fetch.
    if found_count == 0 && transport_count > 0 {
        let sample = sample_transport.unwrap_or_default();
        return Err(format!(
            "could not reach any r-universe host (network down or curl missing): {sample}. \
             Install the packages and use `raven packages freeze` for an offline, version-exact \
             capture, or check connectivity."
        ));
    }

    // Step 10: renv version-skew warnings
    let pinned = read_renv_lock_package_versions(&root.join("renv.lock")).unwrap_or_default();
    for w in renv_skew_warnings(&fetched_records, &pinned) {
        eprintln!("{w}");
    }

    // Step 11: resolved-nowhere warnings
    let mut resolved_nowhere: Vec<String> = seen
        .iter()
        .filter(|n| !existing_names.contains(*n) && !is_installed(n) && !found_names.contains(*n))
        .cloned()
        .collect();
    resolved_nowhere.sort();
    for p in &resolved_nowhere {
        eprintln!(
            "warning: package '{p}' could not be resolved from r-universe; it may be \
             GitHub-only, internal, not yet on r-universe, a typo, or the host was unreachable"
        );
    }

    // Step 12: merge + write
    let mut merged = existing_records.clone();
    merged.extend(fetched_records);
    merged.sort_by(|a, b| a.name.cmp(&b.name));

    let mut existing_sorted = existing_records;
    existing_sorted.sort_by(|a, b| a.name.cmp(&b.name));

    if merged == existing_sorted {
        eprintln!("no changes; left {} untouched", out.display());
    } else {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let r_version =
            if args.missing_only && lib_outcome.as_ref().is_some_and(|o| o.status.is_ready()) {
                "present (--missing-only)".to_string()
            } else {
                "none (fetched)".to_string()
            };
        let db = RepoDb {
            schema_version: REPO_DB_SCHEMA_VERSION,
            provenance: RepoDbProvenance {
                raven_version: env!("CARGO_PKG_VERSION").to_string(),
                r_version,
                generated_unix: now_unix,
            },
            packages: merged,
        };
        write_repo_db_file_atomic(&out, &db)?;
        eprintln!("Wrote {} packages to {}", db.packages.len(), out.display());
    }

    // Step 13: fail-on-missing exit
    if args.fail_on_missing && !resolved_nowhere.is_empty() {
        return Err(format!(
            "{} package(s) could not be resolved from r-universe; failing because \
             --fail-on-missing was set",
            resolved_nowhere.len()
        ));
    }

    Ok(())
}

/// Generate a repo's Tier 2 `.raven/packages.json` from installed packages.
///
/// **Provider-less (decision #6):** built with
/// [`build_package_library_tier1_only`](crate::package_library::build_package_library_tier1_only)
/// so a not-installed package can never leak a Tier 2/3 guess into the frozen
/// file — every candidate is gated by `package_exists` (Tier-1) and the
/// default-attached Base-7 packages are skipped.
///
/// For `--used` the candidate set is **maximally inclusive (decision #10):**
/// `library`/`require`/`loadNamespace` call args ∪ `::`/`:::` LHS ∪ the repo's
/// DESCRIPTION Depends+Imports ∪ renv.lock ∪ transitive Depends. No-op when the
/// resulting records are unchanged (decision #11).
pub async fn run_freeze(args: FreezeArgs) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = match &args.workspace {
        Some(p) => absolute_path(&cwd, p),
        None => cwd.clone(),
    };
    let outcome =
        crate::package_library::build_package_library_tier1_only(None, &[], Some(root.clone()))
            .await;

    // Freeze snapshots the INSTALLED library (Tier 1), so it needs R and at
    // least one library path. Without a ready library, the `package_exists`
    // gate below filters every candidate out and we'd overwrite a good
    // `.raven/packages.json` with an empty/partial snapshot — refuse instead.
    if !outcome.status.is_ready() {
        use crate::package_library::PackageLibraryStatus::*;
        let reason = match &outcome.status {
            RNotFound => "no R interpreter was found".to_string(),
            NoLibraryPaths => "R reported no library paths".to_string(),
            InitFailed(e) => format!("R initialization failed: {e}"),
            Disabled => "package support is disabled".to_string(),
            Ready => unreachable!("guarded by is_ready()"),
        };
        return Err(format!(
            "cannot freeze: {reason}. `raven packages freeze` needs R and an installed \
             library to verify packages; refusing to overwrite {} with an unverified snapshot",
            absolute_path(&root, &args.output).display()
        ));
    }

    let lib = &outcome.library;

    // BFS work-list seeded from the scope's sources. `seen` (below) is the sole
    // dedup mechanism and pop order is irrelevant — the reachable set is a
    // transitive closure and `records` is sorted before writing — so the seeds
    // go straight into `queue` with no intermediate dedup set.
    let mut queue: Vec<String> = Vec::new();
    match args.scope {
        FreezeScope::All => queue.extend(lib.enumerate_installed_packages()),
        FreezeScope::Used => {
            queue.extend(collect_used_package_names(&root)?);
        }
    }

    let mut records: Vec<PackageRecord> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        // Freeze is a local-R, version-exact capture. Skip only the packages
        // Raven treats as always in scope with no `library()` call (the
        // default-attached Base-7), not the full embedded base-priority set.
        // If a user explicitly calls `library(grid)` or runs `--all`, keeping
        // that local-R record is intentional: their R version may differ from
        // the reference R that produced Raven's embedded fallback, and a frozen
        // file that omits explicitly used packages looks like a missed capture.
        if lib.is_base_package(&name) {
            continue;
        }
        if !lib.package_exists(&name) {
            continue;
        }
        if let Some(info) = lib.get_package(&name).await {
            if matches!(args.scope, FreezeScope::Used) {
                for dep in &info.depends {
                    if dep != "R" && !seen.contains(dep) {
                        queue.push(dep.clone());
                    }
                }
            }
            records.push(record_with_version(lib, &name, &info));
        }
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));

    let out = absolute_path(&root, &args.output);
    // `read_repo_db_file` maps a missing file to `Err(Absent)`, so no `exists()`
    // pre-check is needed — an absent/unreadable file simply isn't a no-op match.
    if let Ok(existing) = read_repo_db_file(&out)
        && existing.packages == records
    {
        eprintln!("no changes; left {} untouched", out.display());
        return Ok(());
    }

    let r_version = lib
        .r_subprocess()
        .map(|_| "present".to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let generated_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let db = RepoDb {
        schema_version: REPO_DB_SCHEMA_VERSION,
        provenance: RepoDbProvenance {
            raven_version: env!("CARGO_PKG_VERSION").to_string(),
            r_version,
            generated_unix,
        },
        packages: records,
    };
    write_repo_db_file(&out, &db).map_err(|e| e.to_string())?;
    eprintln!("Wrote {} packages to {}", db.packages.len(), out.display());
    Ok(())
}

/// Compute the union of package names referenced in the workspace: scan R files
/// for `library()`/`require()`/`loadNamespace()`/`requireNamespace()` calls and
/// `::` / `:::` LHS, plus DESCRIPTION Depends+Imports, plus renv.lock names.
fn collect_used_package_names(root: &std::path::Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    names.extend(scan_workspace_referenced_packages(root));
    names.extend(read_description_depends_imports(&root.join("DESCRIPTION")));
    names.extend(read_renv_lock_package_names(&root.join("renv.lock")).map_err(|e| e.to_string())?);
    Ok(names)
}

/// Return the set of base/recommended package names from the embedded database,
/// without requiring an R subprocess.
fn embedded_base_packages() -> std::collections::HashSet<String> {
    crate::package_db::embedded_base::load().1
}

/// Scan every R file under `root` for the maximally-inclusive referenced set
/// (decision #10): `library`/`require`/`loadNamespace`/`requireNamespace` call
/// args ∪ the `::`/`:::` namespace LHS. A single tree walk
/// ([`collect_referenced_packages`]) collects both — covering `requireNamespace`
/// directly rather than relying on `Document::loaded_packages` (whose detector
/// omits it). Over-inclusion is harmless: the `package_exists` gate in
/// `run_freeze` drops anything not installed.
fn scan_workspace_referenced_packages(root: &std::path::Path) -> Vec<String> {
    use tower_lsp::lsp_types::Url;
    let Ok(workspace_url) = Url::from_file_path(root) else {
        return Vec::new();
    };
    let index = crate::state::scan_workspace(std::slice::from_ref(&workspace_url), 0);
    let mut names = std::collections::BTreeSet::new();
    for doc in index.values() {
        if let Some(tree) = &doc.tree {
            let text = doc.contents.to_string();
            collect_referenced_packages(tree, &text, &mut names);
        }
    }
    names.into_iter().collect()
}

/// Collect every referenced package name from one parsed tree (decision #10):
/// - the `namespace_identifier` (LHS) of each `pkg::name` / `pkg:::name`
///   (`namespace_operator` node — the package id is `child(0)`, confirmed against
///   `find_namespace_context` in `handlers.rs`), and
/// - the first argument of each `library`/`require`/`loadNamespace`/
///   `requireNamespace` call (mirrors `state::extract_loaded_packages`, extended
///   to cover `requireNamespace`).
fn collect_referenced_packages(
    tree: &tree_sitter::Tree,
    text: &str,
    out: &mut std::collections::BTreeSet<String>,
) {
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "namespace_operator" => {
                if let Some(id) = node.child(0) {
                    let name = text[id.byte_range()].trim_matches(|c| c == '"' || c == '\'');
                    if is_valid_package_name(name) {
                        out.insert(name.to_string());
                    }
                }
            }
            "call" => {
                if let Some(func) = node.child_by_field_name("function") {
                    let func_text = &text[func.byte_range()];
                    if matches!(
                        func_text,
                        "library" | "require" | "loadNamespace" | "requireNamespace"
                    ) && let Some(args) = node.child_by_field_name("arguments")
                    {
                        for i in 0..args.child_count() {
                            let Some(arg) = args.child(i as u32) else {
                                continue;
                            };
                            if arg.kind() != "argument" {
                                continue;
                            }
                            if let Some(value) = arg.child_by_field_name("value") {
                                let name = text[value.byte_range()]
                                    .trim_matches(|c| c == '"' || c == '\'');
                                if is_valid_package_name(name) {
                                    out.insert(name.to_string());
                                }
                                break; // only the first arg names the package
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        for i in (0..node.child_count()).rev() {
            if let Some(c) = node.child(i as u32) {
                stack.push(c);
            }
        }
    }
}

fn read_description_depends_imports(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = std::collections::BTreeSet::new();
    out.extend(crate::namespace_parser::parse_description_field_pub(
        &text, "Depends",
    ));
    out.extend(crate::namespace_parser::parse_description_field_pub(
        &text, "Imports",
    ));
    // `parse_description_field_pub` already drops the "R" version requirement,
    // so `out` never contains it.
    out.into_iter()
        .filter(|p| is_valid_package_name(p))
        .collect()
}

/// Build a Tier 2/3 record for an installed package, stamping the `Version` that
/// `PackageRecord::from_info` deliberately leaves empty (`PackageInfo` carries no
/// version). Both capture loops funnel through here so a record can never be
/// pushed with an empty version — an empty version sorts lowest in the
/// append-only monotonic merge, so a missed stamp would silently demote a real
/// package.
fn record_with_version(
    lib: &crate::package_library::PackageLibrary,
    name: &str,
    info: &crate::package_library::PackageInfo,
) -> PackageRecord {
    let mut rec = PackageRecord::from_info(info);
    rec.version = lib.package_version(name).unwrap_or_default();
    rec
}

pub async fn run_build_shipped_db(args: BuildShippedDbArgs) -> Result<(), String> {
    let prior: Vec<PackageRecord> = if args.fresh {
        Vec::new()
    } else {
        let seed_path = args.seed.clone().unwrap_or_else(|| args.output.clone());
        match ShippedDb::open(&seed_path) {
            Ok(db) => db.all_records(),
            // A missing seed is the only case safe to treat as "start fresh"
            // (first build / no prior Release). Any OTHER failure (corrupt, or a
            // newer Tier-3 format) means the prior DB's accumulated history would
            // be silently dropped, violating the append-only / version-monotonic
            // contract — so abort rather than ship a regressed (shrunken) DB.
            // `--fresh` is the explicit opt-in for intentionally discarding it.
            Err(crate::package_db::binary_db::ShippedDbError::Absent) => Vec::new(),
            Err(e) => {
                return Err(format!(
                    "could not read seed DB {}: {e}; rerun with --fresh only if dropping \
                     prior history is intentional",
                    seed_path.display()
                ));
            }
        }
    };

    let mut runiverse: Vec<PackageRecord> = Vec::new();
    for (path, min, min_flag) in [
        (
            args.runiverse_cran.as_ref(),
            args.runiverse_cran_min,
            "--runiverse-cran-min",
        ),
        (
            args.runiverse_bioc.as_ref(),
            args.runiverse_bioc_min,
            "--runiverse-bioc-min",
        ),
    ] {
        if let Some(path) = path {
            // A bulk dbdump file is the unverified single artifact; refuse to
            // ingest one without a coverage floor rather than risk shipping a
            // silently-truncated names.db. (A directory is the legacy per-package
            // fixture layout, which carries no such risk and needs no floor.)
            if min.is_none() && !path.is_dir() {
                return Err(format!(
                    "{}: a dbdump file requires a coverage floor; pass {min_flag} (the /api/ls count)",
                    path.display()
                ));
            }
            runiverse.extend(ingest_runiverse_path(path, min).map_err(|e| e.to_string())?);
        }
    }

    let mut reference_r: Vec<PackageRecord> = Vec::new();
    if args.capture_reference {
        // Capture the build machine's entire installed library: the Tier-1-only
        // build auto-detects R and `initialize()` discovers every `.libPaths()`
        // entry, so no library paths need to be passed in. (Tests set
        // `capture_reference = false` to stay deterministic / R-free.)
        let outcome =
            crate::package_library::build_package_library_tier1_only(None, &[], None).await;
        // Reference capture is always on for this maintainer command. A
        // non-ready build or an empty library would silently ship a names.db
        // with no reference capture, so refuse.
        if !outcome.status.is_ready() {
            use crate::package_library::PackageLibraryStatus::*;
            let reason = match &outcome.status {
                RNotFound => "no R interpreter was found".to_string(),
                NoLibraryPaths => "R reported no library paths".to_string(),
                InitFailed(e) => format!("R initialization failed: {e}"),
                Disabled => "package support is disabled".to_string(),
                Ready => unreachable!("guarded by is_ready()"),
            };
            return Err(format!(
                "cannot capture reference R library: {reason}. build-shipped-db needs R \
                 and an installed library for its always-on reference capture"
            ));
        }
        let installed = outcome.library.enumerate_installed_packages();
        if installed.is_empty() {
            return Err(
                "cannot capture reference R library: R reported no installed packages".to_string(),
            );
        }
        for name in installed {
            if let Some(info) = outcome.library.get_package(&name).await {
                reference_r.push(record_with_version(&outcome.library, &name, &info));
            }
        }
    }

    let merged = merge_append_only(prior, runiverse, reference_r);

    // No FORMAT_VERSION bump needed — this post-merge filter removes every
    // package embedded in the binary (all 14 base-priority packages) from all
    // sources before writing the shipped DB, so names.db is strictly non-base.
    let base = embedded_base_packages();
    let merged: Vec<PackageRecord> = merged
        .into_iter()
        .filter(|r| !base.contains(&r.name))
        .collect();

    let provenance = ShippedDbProvenance {
        source: args.source,
        snapshot_date: args.snapshot_date,
        package_count: merged.len() as u32,
        raven_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_shipped_db(&args.output, &merged, provenance).map_err(|e| e.to_string())?;
    eprintln!(
        "Wrote {} packages to {}",
        merged.len(),
        args.output.display()
    );
    Ok(())
}

pub fn run_validate_shipped_db(args: ValidateShippedDbArgs) -> Result<(), String> {
    let db = ShippedDb::open(&args.path).map_err(|e| format!("{}: {e}", args.path.display()))?;
    let records = db.all_records();
    let provenance = db.provenance();
    let expected = provenance.package_count as usize;
    if records.len() != expected {
        return Err(format!(
            "{}: decoded {} package records, but provenance says {}",
            args.path.display(),
            records.len(),
            expected
        ));
    }
    eprintln!(
        "Validated {}: {} packages; source: {}; snapshot: {}; built by Raven {}",
        args.path.display(),
        records.len(),
        provenance.source,
        provenance.snapshot_date,
        provenance.raven_version
    );
    Ok(())
}

pub async fn run_update(args: UpdateArgs) -> Result<(), String> {
    let dest_dir = match args.dest_dir {
        Some(dir) => dir,
        None => crate::package_db::user_data_sidecar_path("names.db")
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .ok_or_else(|| {
                "could not determine Raven user-data directory; rerun with --dest-dir, or set \
                 RAVEN_NAMES_DB to override the database location"
                    .to_string()
            })?,
    };

    let names_bytes = download_asset(&args.base_url, "names.db").await?;
    let installed = install_downloaded_sidecars(&dest_dir, names_bytes)?;
    eprintln!(
        "Installed names.db to {}",
        installed.names_db_path.display()
    );
    eprintln!(
        "names.db source: {}; snapshot: {}; packages: {}",
        installed.names_db_provenance.source,
        installed.names_db_provenance.snapshot_date,
        installed.names_db_provenance.package_count
    );
    Ok(())
}

async fn download_asset(base_url: &str, name: &str) -> Result<Vec<u8>, String> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), name);
    tokio::task::spawn_blocking(move || download_asset_blocking(&url))
        .await
        .map_err(|e| format!("download task failed for {name}: {e}"))?
}

/// Run curl with the shared arg list. Both `download_asset_blocking` and
/// `fetch_one_package_blocking` use this to avoid arg-list drift.
fn run_curl(url: &str) -> std::io::Result<std::process::Output> {
    Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--connect-timeout",
            "20",
            "--max-time",
            "300",
            "--max-filesize",
            "209715200",
            url,
        ])
        .output()
}

fn download_asset_blocking(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!(
            "refusing to download {url}: only http:// and https:// URLs are supported. {}",
            manual_sidecar_guidance()
        ));
    }

    let output = run_curl(url).map_err(|e| {
        format!(
            "failed to run curl for {url}: {e}. `raven packages update` requires curl; \
             {}",
            manual_sidecar_guidance()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(curl_failure_error(
            url,
            &output.status.to_string(),
            stderr.trim(),
        ));
    }
    Ok(output.stdout)
}

/// Outcome of fetching one package across the ordered base-url list.
///
/// The distinction between `NotFound` and `Transport` is critical for Task 3's
/// error policy: a lone typo (NotFound) must never hard-error the command; only
/// genuine infrastructure failures (Transport) can escalate to a hard error when
/// nothing was fetched at all.
#[derive(Debug)]
enum PackageFetchOutcome {
    /// Fetched and parsed from one of the bases.
    Found(PackageRecord),
    /// Every base served a genuine HTTP not-found (404/410): the package is
    /// off-ecosystem or a typo. SOFT — becomes a resolved-nowhere warning in
    /// Task 3. Only a true not-found qualifies; a 5xx is a transient outage and
    /// is treated as `Transport` so it can't silently drop a real package.
    NotFound,
    /// curl could not run, could not reach any base (spawn failure / DNS /
    /// connection refused / timeout), or every base returned a *server* error
    /// (5xx) — i.e. an infrastructure failure, surfaced so the command can
    /// report it rather than silently dropping the package. Carries a message.
    Transport(String),
}

/// Whether a curl `--fail` HTTP error (exit 22) is a genuine "not found".
///
/// curl with `--show-error` emits `curl: (22) The requested URL returned error:
/// NNN ...` on stderr. A 404/410 means the package is not served by this host
/// (soft `NotFound`); any other status — notably a 5xx during an r-universe
/// outage — must escalate to `Transport` so a transient failure can't silently
/// write a database that drops a real package. When the status can't be parsed
/// we conservatively return `false` (treat as `Transport`, the loud direction).
fn curl_http_error_is_not_found(stderr: &str) -> bool {
    stderr
        .split("returned error:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| code == 404 || code == 410)
}

/// Fetch one package's r-universe JSON, trying each `{base}/api/packages/{pkg}`
/// in order (CRAN then Bioc). Returns Found on the first base that serves a
/// parseable record; NotFound when every base returned a genuine HTTP not-found
/// (404/410); Transport when curl could not reach any base, or a base failed
/// with a non-not-found error (5xx, parse failure, spawn failure).
fn fetch_one_package_blocking(pkg: &str, base_urls: &[String]) -> PackageFetchOutcome {
    use crate::package_db::runiverse::parse_runiverse_json;

    let mut saw_http_not_found = false;
    let mut last_error: Option<String> = None;

    for base in base_urls {
        let url = format!("{}/api/packages/{}", base.trim_end_matches('/'), pkg);
        match run_curl(&url) {
            Err(e) => {
                last_error = Some(format!("failed to run curl for {url}: {e}"));
            }
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                match parse_runiverse_json(&text) {
                    Ok(rec) => return PackageFetchOutcome::Found(rec),
                    Err(e) => {
                        last_error = Some(format!("parse error for {url}: {e}"));
                    }
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if output.status.code() == Some(22) && curl_http_error_is_not_found(&stderr) {
                    saw_http_not_found = true;
                } else {
                    last_error = Some(stderr.trim().to_string());
                }
            }
        }
    }

    // Only a clean not-found across the bases is soft: if ANY base failed for a
    // non-not-found reason (5xx outage, parse error, unreachable), that evidence
    // wins, so a 404 on one host can't mask a transient outage on another and
    // silently drop a package that the outaged host might actually serve.
    if saw_http_not_found && last_error.is_none() {
        PackageFetchOutcome::NotFound
    } else {
        PackageFetchOutcome::Transport(
            last_error.unwrap_or_else(|| format!("could not reach any r-universe base for {pkg}")),
        )
    }
}

const MAX_CONCURRENT_FETCHES: usize = 8;

/// Fetch a batch (one BFS wave) of packages concurrently, bounded to
/// MAX_CONCURRENT_FETCHES. curl is blocking, so each fetch runs on a blocking
/// task; a semaphore caps in-flight work. Returns each name paired with its
/// outcome (order not guaranteed).
async fn fetch_packages_wave(
    names: Vec<String>,
    base_urls: &[String],
) -> Vec<(String, PackageFetchOutcome)> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
    let urls: Arc<Vec<String>> = Arc::new(base_urls.to_vec());
    let mut set = JoinSet::new();

    for name in names {
        let sem = Arc::clone(&sem);
        let urls = Arc::clone(&urls);
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let n = name.clone();
            let result = tokio::task::spawn_blocking(move || fetch_one_package_blocking(&n, &urls))
                .await
                .unwrap();
            (name, result)
        });
    }

    let mut results = Vec::new();
    while let Some(res) = set.join_next().await {
        results.push(res.unwrap());
    }
    results
}

fn curl_failure_error(url: &str, status: &str, stderr: &str) -> String {
    format!(
        "curl failed to download {url} with status {status}: {stderr}. {}",
        manual_sidecar_guidance()
    )
}

fn manual_sidecar_guidance() -> &'static str {
    "Alternatively, set RAVEN_NAMES_DB to a manually installed names.db"
}

pub fn install_downloaded_sidecars(
    dest_dir: &Path,
    names_bytes: Vec<u8>,
) -> Result<InstalledSidecars, String> {
    std::fs::create_dir_all(dest_dir).map_err(|e| sidecar_write_error(dest_dir, "create", e))?;
    let names_final = dest_dir.join("names.db");

    let names_tmp = write_unique_temp(dest_dir, "names.db", names_bytes)?;

    let db = ShippedDb::open(&names_tmp).map_err(|e| {
        let _ = std::fs::remove_file(&names_tmp);
        format!(
            "downloaded names.db failed validation: {e}. {}",
            manual_sidecar_guidance()
        )
    })?;
    let names_db_provenance = db.provenance().clone();
    drop(db);

    // Backup existing, replace, restore on failure. Clean up the validated
    // temp on any failure after validation so a rare error can't leak it.
    let backup = backup_existing_final(&names_final).inspect_err(|_e| {
        let _ = std::fs::remove_file(&names_tmp);
    })?;
    if let Err(e) = replace_with_tmp(&names_tmp, &names_final) {
        let _ = std::fs::remove_file(&names_tmp);
        restore_backup(backup.as_deref(), &names_final);
        return Err(e);
    }
    remove_backup(backup.as_deref());

    Ok(InstalledSidecars {
        names_db_path: names_final,
        names_db_provenance,
    })
}

fn write_unique_temp(dest_dir: &Path, prefix: &str, bytes: Vec<u8>) -> Result<PathBuf, String> {
    for attempt in 0..100_u32 {
        let path = unique_sidecar_path(dest_dir, prefix, attempt, "tmp");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&bytes) {
                    let _ = std::fs::remove_file(&path);
                    return Err(sidecar_write_error(&path, "write", e));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(sidecar_write_error(&path, "create", e)),
        }
    }

    Err(format!(
        "could not create unique temporary database file in {}; rerun with --dest-dir pointing to a \
         writable directory, or set RAVEN_NAMES_DB to override the database location",
        dest_dir.display()
    ))
}

fn backup_existing_final(final_path: &Path) -> Result<Option<PathBuf>, String> {
    if !final_path.exists() {
        return Ok(None);
    }
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sidecar");
    let prefix = format!("{file_name}.backup");

    for attempt in 0..100_u32 {
        let backup = unique_sidecar_path(dir, &prefix, attempt, "bak");
        match std::fs::rename(final_path, &backup) {
            Ok(()) => return Ok(Some(backup)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(sidecar_write_error(final_path, "backup", e)),
        }
    }

    Err(format!(
        "could not create unique backup database file for {}; rerun with --dest-dir pointing to a \
         writable directory, or set RAVEN_NAMES_DB to override the database location",
        final_path.display()
    ))
}

fn unique_sidecar_path(dest_dir: &Path, prefix: &str, attempt: u32, suffix: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dest_dir.join(format!(".{prefix}.{pid}.{nanos}.{attempt}.{suffix}"))
}

fn restore_backup(backup: Option<&Path>, final_path: &Path) {
    if let Some(backup) = backup {
        let _ = std::fs::rename(backup, final_path);
    }
}

fn remove_backup(backup: Option<&Path>) {
    if let Some(backup) = backup {
        let _ = std::fs::remove_file(backup);
    }
}

/// Atomically replace `final_path` with the already-written, already-validated
/// `tmp` file.
///
/// This deliberately hand-rolls the rename instead of using
/// `tempfile::NamedTempFile::persist`: the install needs explicit rename control
/// to back up and roll back the sidecar, and on Windows it uses
/// `MOVEFILE_WRITE_THROUGH` so the replacement is flushed before returning —
/// neither of which `persist` offers. `write_unique_temp` keeps the temp on the
/// same directory/filesystem so the rename stays atomic.
fn replace_with_tmp(tmp: &Path, final_path: &Path) -> Result<(), String> {
    replace_file(tmp, final_path).map_err(|e| sidecar_write_error(final_path, "replace", e))
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, final_path: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, final_path)
}

#[cfg(windows)]
fn replace_file(tmp: &Path, final_path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let existing: Vec<u16> = tmp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let new: Vec<u16> = final_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sidecar_write_error(path: &Path, action: &str, error: std::io::Error) -> String {
    format!(
        "could not {action} {}: {error}; rerun with --dest-dir pointing to a writable \
         directory, or set RAVEN_NAMES_DB to override the database location",
        path.display()
    )
}

pub struct BuildEmbeddedBaseArgs {
    pub reference_lib: PathBuf,
    pub output: PathBuf,
}

pub fn parse_build_embedded_base_args(
    mut argv: impl Iterator<Item = String>,
) -> Result<BuildEmbeddedBaseArgs, String> {
    let mut reference_lib = None;
    let mut output = PathBuf::from("crates/raven/src/package_db/embedded_base_generated.rs");
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--reference-lib" => {
                reference_lib = Some(PathBuf::from(
                    argv.next().ok_or("--reference-lib needs a path")?,
                ))
            }
            "--output" => output = PathBuf::from(argv.next().ok_or("--output needs a path")?),
            "--help" => return Err("HELP".into()),
            s => return Err(format!("unknown flag: {s}")),
        }
    }
    Ok(BuildEmbeddedBaseArgs {
        reference_lib: reference_lib.ok_or("--reference-lib is required")?,
        output,
    })
}

/// One captured base package: `(name, exports, datasets, depends)`.
type EmbeddedPkgCapture = (String, Vec<String>, Vec<String>, Vec<String>);

/// Emit the `// @generated` `embedded_base_generated.rs` source from captured
/// per-package `(name, exports, datasets, depends)` buckets. Each name is
/// rendered with `{:?}` so exotic/operator identifiers escape correctly.
fn emit_embedded_base_source(pkgs: &[EmbeddedPkgCapture], r_version: &str) -> String {
    fn arr(items: &[String]) -> String {
        let parts: Vec<String> = items.iter().map(|s| format!("{s:?}")).collect();
        format!("&[{}]", parts.join(", "))
    }
    let mut out = String::from(
        "// @generated by `raven packages build-embedded-base` — DO NOT EDIT BY HAND.\n",
    );
    out.push_str(&format!("// Reference R version: {r_version}\n"));
    out.push_str("static EMBEDDED_BASE_PACKAGES: &[EmbeddedBasePackage] = &[\n");
    for (name, exports, datasets, depends) in pkgs {
        out.push_str(&format!(
            "    EmbeddedBasePackage {{ name: {name:?}, exports: {}, datasets: {}, depends: {} }},\n",
            arr(exports), arr(datasets), arr(depends),
        ));
    }
    out.push_str("];\n");
    out
}

/// Authoritative dataset object names for one base package via R's `data()`.
/// `data()` Items are formatted `obj (topic)` for datasets documented under a
/// shared topic, so the leading token is the resolvable object name (e.g.
/// `state.abb`, `euro.cross`). Returns empty if R errors or the package ships
/// no data. `pkg` is a fixed base-priority identifier, so the interpolation is safe.
async fn capture_base_datasets(r: &crate::r_subprocess::RSubprocess, pkg: &str) -> Vec<String> {
    let code = format!(
        "items <- suppressWarnings(data(package=\"{pkg}\")$results); \
         if (length(items)) writeLines(sub(\" .*$\", \"\", items[, \"Item\"]))"
    );
    match r.execute_r_code(&code).await {
        Ok(out) => out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Maintainer-only: captures the 14 base-priority packages from a reference R
/// library and emits `embedded_base_generated.rs`. When R is present, exports
/// come from `getNamespaceExports` and datasets from `data()` (both
/// authoritative); a fresh, non-initialized `PackageLibrary` supplies `Depends`
/// and the R-absent fallbacks (static NAMESPACE exports, INDEX dataset topics).
pub async fn run_build_embedded_base(args: BuildEmbeddedBaseArgs) -> Result<(), String> {
    use crate::package_library::PackageLibrary;
    let query_r = crate::r_subprocess::RSubprocess::new(None);
    let r_version = match &query_r {
        Some(sub) => sub
            .execute_r_code("cat(as.character(getRversion()))")
            .await
            .unwrap_or_else(|_| "unknown".to_string()),
        None => "unknown".to_string(),
    };
    // Fresh library (NOT initialize()d) so `get_package` keeps datasets out of
    // the exports set; its own R subprocess expands base exportPattern.
    let mut lib = PackageLibrary::with_subprocess(crate::r_subprocess::RSubprocess::new(None));
    lib.set_lib_paths(vec![args.reference_lib.clone()]);
    let mut pkgs = Vec::new();
    for name in crate::r_subprocess::get_base_priority_packages() {
        let info = lib.get_package(&name).await.ok_or_else(|| {
            format!(
                "base package {name} not found under {}",
                args.reference_lib.display()
            )
        })?;
        let mut exports: Vec<String> = match &query_r {
            // Authoritative namespace exports (e.g. utils = 237). The static
            // NAMESPACE parse used by `get_package` for non-exportPattern base
            // packages is a bloated superset that even leaks `##`-commented
            // lines, so prefer R's `getNamespaceExports` when R is present.
            Some(sub) => match sub.get_package_exports(&name).await {
                Ok(v) => v,
                Err(_) => info.exports.iter().cloned().collect(),
            },
            None => info.exports.iter().cloned().collect(),
        };
        // Datasets: `info.lazy_data` (parse_data_symbols) only sees individual
        // data files; base packages like `datasets` bundle their data in
        // `Rdata.r{db,dx,ds}`, so the object names are knowable only via R's
        // `data()` (authoritative, e.g. `state.abb`/`euro.cross`). When R is
        // unavailable, fall back to INDEX topics for any package with a `data/`
        // dir (lower fidelity — topic names, not object names).
        let mut dataset_set: std::collections::HashSet<String> =
            info.lazy_data.iter().cloned().collect();
        match &query_r {
            Some(sub) => dataset_set.extend(capture_base_datasets(sub, &name).await),
            None => {
                let pkg_dir = args.reference_lib.join(&name);
                if std::fs::symlink_metadata(pkg_dir.join("data"))
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                    && let Ok(idx) = crate::namespace_parser::parse_index_exports(&pkg_dir).await
                {
                    dataset_set.extend(idx);
                }
            }
        }
        let mut datasets: Vec<String> = dataset_set.into_iter().collect();
        let mut depends = info.depends.clone();
        exports.sort();
        datasets.sort();
        depends.sort();
        pkgs.push((name, exports, datasets, depends));
    }
    // Deterministic output regardless of the source order of base-package names.
    pkgs.sort_by(|a, b| a.0.cmp(&b.0));
    let src = emit_embedded_base_source(&pkgs, &r_version);
    std::fs::write(&args.output, src).map_err(|e| e.to_string())?;
    eprintln!("Wrote embedded base table to {}", args.output.display());
    Ok(())
}

/// Dispatch the `packages` subcommand group on its second token.
pub async fn run(mut argv: impl Iterator<Item = String>) -> Result<(), String> {
    match argv.next().as_deref() {
        Some("--help" | "help") => {
            print_help();
            Ok(())
        }
        Some("update") => {
            match parse_update_args(argv) {
                Ok(args) => run_update(args).await,
                Err(msg) if msg == "HELP" => {
                    print_update_help();
                    Ok(())
                }
                Err(msg) => Err(msg),
            }
        }
        Some("freeze") => {
            match parse_freeze_args(argv) {
                Ok(args) => run_freeze(args).await,
                Err(msg) if msg == "HELP" => {
                    print_freeze_help();
                    Ok(())
                }
                Err(msg) => Err(msg),
            }
        }
        Some("fetch") => {
            match parse_fetch_args(argv) {
                Ok(args) => run_fetch(args).await,
                Err(msg) if msg == "HELP" => {
                    print_fetch_help();
                    Ok(())
                }
                Err(msg) => Err(msg),
            }
        }
        Some("build-shipped-db") => {
            match parse_build_shipped_db_args(argv) {
                Ok(args) => run_build_shipped_db(args).await,
                Err(msg) if msg == "HELP" => {
                    print_build_shipped_db_help();
                    Ok(())
                }
                Err(msg) => Err(msg),
            }
        }
        Some("build-embedded-base") => {
            match parse_build_embedded_base_args(argv) {
                Ok(args) => run_build_embedded_base(args).await,
                Err(msg) if msg == "HELP" => {
                    print_build_embedded_base_help();
                    Ok(())
                }
                Err(msg) => Err(msg),
            }
        }
        Some("validate-shipped-db") => {
            match parse_validate_shipped_db_args(argv) {
                Ok(args) => run_validate_shipped_db(args),
                Err(msg) if msg == "HELP" => {
                    print_validate_shipped_db_help();
                    Ok(())
                }
                Err(msg) => Err(msg),
            }
        }
        Some(other) => Err(format!("unknown packages subcommand: {other}")),
        None => {
            Err("usage: raven packages <fetch|freeze|update|build-shipped-db|build-embedded-base|validate-shipped-db> [OPTIONS]".into())
        }
    }
}

pub fn print_help() {
    println!(
        "raven packages — package-database commands\n\n\
         Usage:\n  \
         raven packages fetch [--missing-only] [--fail-on-missing] [--output PATH] \
[--workspace DIR] [--base-urls URL[,URL]]\n  \
         raven packages freeze [--used|--installed|--all] [--output PATH] [--workspace DIR]\n  \
         raven packages update [YYYY-MM-DD | --base-url URL] [--dest-dir DIR]\n  \
         raven packages build-shipped-db [--runiverse-cran DIR|dbdump.bson [--runiverse-cran-min N]] \
[--runiverse-bioc DIR|dbdump.bson [--runiverse-bioc-min N]] [--seed names.db | --fresh] --output names.db \
[--snapshot-date S] [--source S]\n  \
         raven packages build-embedded-base --reference-lib DIR [--output PATH]\n  \
         raven packages validate-shipped-db names.db\n\n\
         Run `raven packages <subcommand> --help` for command-specific help.\n\
         Docs: {}\n",
        env!("CARGO_PKG_REPOSITORY")
    );
}

pub fn print_fetch_help() {
    println!(
        "raven packages fetch — fetch package exports from r-universe\n\n\
         Usage: raven packages fetch [OPTIONS]\n\n\
         Produces .raven/packages.json from CRAN/Bioconductor r-universe metadata.\n\
         Existing records are preserved and new records are added for packages the\n\
         workspace uses but the file does not already cover.\n\n\
         Options:\n  \
         --missing-only              Skip packages already installed locally when R is available\n  \
         --fail-on-missing           Exit non-zero if any used package cannot be resolved\n  \
         --output PATH               Output path (default: .raven/packages.json)\n  \
         --workspace DIR             Workspace root to scan (default: current directory)\n  \
         --base-urls URL[,URL]       Ordered r-universe hosts\n  \
         --help                      Print this help message\n\n\
         Alias: raven fetch [OPTIONS]\n\
         Docs: {}\n",
        env!("CARGO_PKG_REPOSITORY")
    );
}

pub fn print_freeze_help() {
    println!(
        "raven packages freeze — capture installed package exports\n\n\
         Usage: raven packages freeze [OPTIONS]\n\n\
         Writes .raven/packages.json from packages installed in the local R library.\n\
         Commit this file when CI should use version-pinned package export metadata\n\
         without downloading a broad database.\n\n\
         Options:\n  \
         --used                      Capture packages used by the workspace (default)\n  \
         --installed, --all          Capture every installed package\n  \
         --output PATH               Output path (default: .raven/packages.json)\n  \
         --workspace DIR             Workspace root to scan (default: current directory)\n  \
         --help                      Print this help message\n\n\
         Alias: raven freeze [OPTIONS]\n\
         Docs: {}\n",
        env!("CARGO_PKG_REPOSITORY")
    );
}

pub fn print_update_help() {
    println!(
        "raven packages update — download Raven's package symbol database\n\n\
         Usage: raven packages update [YYYY-MM-DD | --base-url URL] [OPTIONS]\n\n\
         Downloads names.db, Raven's broad CRAN/Bioconductor package symbol database.\n\
         Use this in CI or local installs when R packages are not installed but Raven\n\
         should still recognize exported package symbols.\n\n\
         Options:\n  \
         --base-url URL              Release asset base URL to download from\n  \
         --dest-dir DIR              Destination directory for names.db\n  \
         --help                      Print this help message\n\n\
         Docs: {}\n",
        env!("CARGO_PKG_REPOSITORY")
    );
}

pub fn print_build_shipped_db_help() {
    println!(
        "raven packages build-shipped-db — maintainer package database builder\n\n\
         Usage: raven packages build-shipped-db [OPTIONS] --output names.db\n\n\
         Options:\n  \
         --runiverse-cran PATH       CRAN r-universe dbdump BSON or package JSON directory\n  \
         --runiverse-cran-min N      Minimum distinct CRAN packages required\n  \
         --runiverse-bioc PATH       Bioconductor r-universe dbdump BSON or package JSON directory\n  \
         --runiverse-bioc-min N      Minimum distinct Bioconductor packages required\n  \
         --seed names.db             Prior database seed\n  \
         --fresh, --no-seed          Build without a prior database seed\n  \
         --output names.db           Output database path (required)\n  \
         --snapshot-date S           Snapshot date recorded in provenance\n  \
         --source S                  Source label recorded in provenance\n  \
         --help                      Print this help message\n"
    );
}

pub fn print_build_embedded_base_help() {
    println!(
        "raven packages build-embedded-base — maintainer embedded base table builder\n\n\
         Usage: raven packages build-embedded-base --reference-lib DIR [OPTIONS]\n\n\
         Options:\n  \
         --reference-lib DIR         Reference R library containing base-priority packages (required)\n  \
         --output PATH               Generated Rust source path\n  \
         --help                      Print this help message\n"
    );
}

pub fn print_validate_shipped_db_help() {
    println!(
        "raven packages validate-shipped-db — validate a names.db database\n\n\
         Usage: raven packages validate-shipped-db names.db\n\n\
         Opens and fully decodes a shipped database, checking container metadata,\n\
         format compatibility, checksums, index bounds, and record count.\n\n\
         Options:\n  \
         --help                      Print this help message\n"
    );
}

#[cfg(test)]
mod tests {
    use crate::package_db::binary_db::{ShippedDb, ShippedDbProvenance, write_shipped_db};
    use crate::package_db::json_db::{
        REPO_DB_SCHEMA_VERSION, RepoDb, RepoDbProvenance, read_repo_db_file, write_repo_db_file,
    };
    use crate::package_db::model::PackageRecord;

    #[test]
    fn parse_freeze_args_defaults_to_used() {
        let a = super::parse_freeze_args(std::iter::empty()).unwrap();
        assert_eq!(a.scope, super::FreezeScope::Used);
        assert_eq!(a.output, std::path::PathBuf::from(".raven/packages.json"));

        let b = super::parse_freeze_args(["--all".to_string()].into_iter()).unwrap();
        assert_eq!(b.scope, super::FreezeScope::All);

        let c = super::parse_freeze_args(
            [
                "--installed".to_string(),
                "--output".to_string(),
                "x.json".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(c.scope, super::FreezeScope::All);
        assert_eq!(c.output, std::path::PathBuf::from("x.json"));
    }

    #[test]
    fn collect_referenced_packages_finds_namespace_and_calls() {
        // R-free: parse a snippet with the tree-sitter R grammar and assert both
        // `::`/`:::` LHS AND library/require/loadNamespace/requireNamespace call
        // args are collected. Proves detection (incl. requireNamespace, the
        // decision-#10 gap) without any installed package.
        let src = "dplyr::mutate(x)\n\
                   utils:::internal()\n\
                   library(ggplot2)\n\
                   require(tibble)\n\
                   loadNamespace(\"jsonlite\")\n\
                   requireNamespace(\"rlang\")\n\
                   bare_var\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .expect("load R grammar");
        let tree = parser.parse(src, None).expect("parse R snippet");

        let mut out = std::collections::BTreeSet::new();
        super::collect_referenced_packages(&tree, src, &mut out);

        for expected in ["dplyr", "utils", "ggplot2", "tibble", "jsonlite", "rlang"] {
            assert!(out.contains(expected), "expected {expected}, got {out:?}");
        }
        assert!(
            !out.contains("bare_var"),
            "bare identifier must not be collected, got {out:?}"
        );
    }

    #[test]
    fn parse_build_shipped_db_args() {
        let args = super::parse_build_shipped_db_args(
            [
                "--runiverse-cran".to_string(),
                "cran".to_string(),
                "--runiverse-cran-min".to_string(),
                "24000".to_string(),
                "--runiverse-bioc".to_string(),
                "bioc".to_string(),
                "--output".to_string(),
                "out.db".to_string(),
                "--fresh".to_string(),
                "--snapshot-date".to_string(),
                "2026-05-30".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(args.runiverse_cran, Some(std::path::PathBuf::from("cran")));
        assert_eq!(args.runiverse_bioc, Some(std::path::PathBuf::from("bioc")));
        assert_eq!(args.runiverse_cran_min, Some(24000));
        // A min is optional per universe: bioc was given a path but no floor.
        assert_eq!(args.runiverse_bioc_min, None);
        assert_eq!(args.output, std::path::PathBuf::from("out.db"));
        assert!(args.fresh, "--fresh skips the default prior-DB seed");
        assert_eq!(args.snapshot_date, "2026-05-30");
        assert!(args.capture_reference);
        assert_eq!(args.seed, None);
    }

    #[test]
    fn parse_build_shipped_db_rejects_non_integer_min() {
        let err = super::parse_build_shipped_db_args(
            [
                "--runiverse-cran-min".to_string(),
                "lots".to_string(),
                "--output".to_string(),
                "out.db".to_string(),
            ]
            .into_iter(),
        )
        .unwrap_err();
        assert!(
            err.contains("--runiverse-cran-min") && err.contains("integer"),
            "expected an integer-parse error, got: {err}"
        );
    }

    #[test]
    fn parse_build_shipped_db_rejects_orphaned_runiverse_min() {
        for (min_flag, path_flag) in [
            ("--runiverse-cran-min", "--runiverse-cran"),
            ("--runiverse-bioc-min", "--runiverse-bioc"),
        ] {
            let err = super::parse_build_shipped_db_args(
                [
                    min_flag.to_string(),
                    "1".to_string(),
                    "--output".to_string(),
                    "out.db".to_string(),
                ]
                .into_iter(),
            )
            .unwrap_err();
            assert_eq!(err, format!("{min_flag} requires {path_flag}"));
        }
    }

    #[test]
    fn parse_build_shipped_db_rejects_zero_min() {
        // A floor of 0 is met by any distinct count, silently disabling the
        // coverage gate — the parser must refuse it even on a direct invocation.
        let err = super::parse_build_shipped_db_args(
            [
                "--runiverse-cran".to_string(),
                "cran".to_string(),
                "--runiverse-cran-min".to_string(),
                "0".to_string(),
                "--output".to_string(),
                "out.db".to_string(),
            ]
            .into_iter(),
        )
        .unwrap_err();
        assert!(
            err.contains("--runiverse-cran-min") && err.contains("positive"),
            "expected a positive-integer error, got: {err}"
        );
    }

    #[test]
    fn parse_validate_shipped_db_requires_one_path() {
        let err = super::parse_validate_shipped_db_args(std::iter::empty()).unwrap_err();
        assert!(err.contains("needs a names.db path"));

        let args =
            super::parse_validate_shipped_db_args(["dist/names.db"].into_iter().map(String::from))
                .unwrap();
        assert_eq!(args.path, std::path::PathBuf::from("dist/names.db"));

        let err =
            super::parse_validate_shipped_db_args(["a.db", "b.db"].into_iter().map(String::from))
                .unwrap_err();
        assert!(err.contains("unexpected extra argument"));
    }

    #[test]
    fn validate_shipped_db_accepts_valid_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        let recs = vec![PackageRecord {
            name: "dplyr".into(),
            version: "1.1.4".into(),
            exports: vec!["mutate".into()],
            depends: vec![],
            lazy_data: vec![],
        }];
        let prov = ShippedDbProvenance {
            source: "t".into(),
            snapshot_date: "2026-06-01".into(),
            package_count: 1,
            raven_version: "9.9.9".into(),
        };
        write_shipped_db(&path, &recs, prov).unwrap();

        super::run_validate_shipped_db(super::ValidateShippedDbArgs { path }).unwrap();
    }

    #[test]
    fn validate_shipped_db_rejects_corrupt_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        std::fs::write(&path, b"NOT A RAVEN DB").unwrap();

        let err =
            super::run_validate_shipped_db(super::ValidateShippedDbArgs { path }).unwrap_err();
        assert!(err.contains("bad magic"), "got {err}");
    }

    // Covers the ONLY logic the validator adds over `ShippedDb::open`: a
    // structurally valid DB whose provenance package_count disagrees with the
    // decoded record count must be rejected.
    #[test]
    fn validate_shipped_db_rejects_provenance_count_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.db");
        let recs = vec![PackageRecord {
            name: "dplyr".into(),
            version: "1.1.4".into(),
            exports: vec!["mutate".into()],
            depends: vec![],
            lazy_data: vec![],
        }];
        let prov = ShippedDbProvenance {
            source: "t".into(),
            snapshot_date: "2026-06-01".into(),
            package_count: 2, // lies: only one record is written
            raven_version: "9.9.9".into(),
        };
        write_shipped_db(&path, &recs, prov).unwrap();

        let err =
            super::run_validate_shipped_db(super::ValidateShippedDbArgs { path }).unwrap_err();
        assert!(err.contains("provenance says"), "got {err}");
    }

    #[tokio::test]
    async fn build_shipped_db_excludes_base_priority_packages() {
        use crate::package_db::binary_db::ShippedDb;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("names.db");
        // Seed a DB containing a base package + a non-base package.
        let seed = dir.path().join("seed.db");
        let recs = vec![
            PackageRecord {
                name: "base".into(),
                version: "4.4.0".into(),
                exports: vec!["c".into()],
                depends: vec![],
                lazy_data: vec![],
            },
            PackageRecord {
                name: "dplyr".into(),
                version: "1.1.4".into(),
                exports: vec!["mutate".into()],
                depends: vec![],
                lazy_data: vec![],
            },
            PackageRecord {
                name: "grid".into(),
                version: "4.4.0".into(),
                exports: vec!["gpar".into()],
                depends: vec![],
                lazy_data: vec![],
            },
        ];
        let prov = ShippedDbProvenance {
            source: "t".into(),
            snapshot_date: "2026-06-01".into(),
            package_count: 3,
            raven_version: "9.9.9".into(),
        };
        write_shipped_db(&seed, &recs, prov).unwrap();

        super::run_build_shipped_db(super::BuildShippedDbArgs {
            capture_reference: false,
            runiverse_cran: None,
            runiverse_bioc: None,
            runiverse_cran_min: None,
            runiverse_bioc_min: None,
            fresh: false,
            seed: Some(seed),
            output: out.clone(),
            snapshot_date: "2026-06-01".into(),
            source: "t".into(),
        })
        .await
        .unwrap();

        let db = ShippedDb::open(&out).unwrap();
        let names: Vec<String> = db.all_records().into_iter().map(|r| r.name).collect();
        assert!(names.contains(&"dplyr".to_string()));
        assert!(
            !names.contains(&"base".to_string()),
            "attached base package must be excluded"
        );
        assert!(
            !names.contains(&"grid".to_string()),
            "non-attached base-priority package must also be excluded"
        );
    }

    #[tokio::test]
    async fn build_shipped_db_rejects_floorless_dbdump() {
        // A dbdump *file* source without a coverage floor must hard-fail before
        // any ingest — shipping an unguarded (possibly truncated) single artifact
        // is the degraded-names.db outcome the gate exists to prevent. The check
        // fires on the path being a non-directory, so a not-yet-existing .bson
        // path exercises it without needing a real dump or R.
        let dir = tempfile::tempdir().unwrap();
        let err = super::run_build_shipped_db(super::BuildShippedDbArgs {
            capture_reference: false,
            runiverse_cran: Some(dir.path().join("cran.bson")),
            runiverse_bioc: None,
            runiverse_cran_min: None, // no floor → must be rejected
            runiverse_bioc_min: None,
            fresh: true,
            seed: None,
            output: dir.path().join("names.db"),
            snapshot_date: "2026-06-01".into(),
            source: "t".into(),
        })
        .await
        .unwrap_err();
        assert!(
            err.contains("coverage floor") && err.contains("--runiverse-cran-min"),
            "expected a floor-required error, got: {err}"
        );
        assert!(
            !dir.path().join("names.db").exists(),
            "must not write a DB when the source is rejected"
        );
    }

    #[test]
    fn parse_update_args_defaults_to_names_db_release() {
        let args = super::parse_update_args(std::iter::empty()).unwrap();
        assert!(args.base_url.contains("github.com"));
        assert_eq!(args.dest_dir, None);
    }

    #[test]
    fn parse_update_args_accepts_base_url_and_dest_dir() {
        let args = super::parse_update_args(
            [
                "--base-url".to_string(),
                "http://127.0.0.1:9/assets".to_string(),
                "--dest-dir".to_string(),
                "/tmp/raven-db".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(args.base_url, "http://127.0.0.1:9/assets");
        assert_eq!(
            args.dest_dir.unwrap(),
            std::path::PathBuf::from("/tmp/raven-db")
        );
    }

    #[test]
    fn parse_update_args_accepts_dated_release() {
        let args = super::parse_update_args(["2026-06-02"].into_iter().map(String::from)).unwrap();
        assert!(
            args.base_url.ends_with("names-db-2026-06-02"),
            "got {}",
            args.base_url
        );
    }

    #[test]
    fn parse_update_args_rejects_date_and_base_url_together() {
        let err = super::parse_update_args(
            ["2026-06-02", "--base-url", "http://x/y"]
                .into_iter()
                .map(String::from),
        )
        .unwrap_err();
        assert!(err.contains("not both"), "got {err}");
    }

    #[test]
    fn parse_update_args_rejects_malformed_date() {
        let err = super::parse_update_args(["2026-6-2"].into_iter().map(String::from)).unwrap_err();
        assert!(err.contains("YYYY-MM-DD"), "got {err}");
    }

    #[test]
    fn parse_update_args_rejects_impossible_date() {
        let err =
            super::parse_update_args(["2026-02-31"].into_iter().map(String::from)).unwrap_err();
        assert!(err.contains("YYYY-MM-DD"), "got {err}");
    }

    #[test]
    fn atomic_install_rejects_invalid_names_db_and_leaves_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("names.db");
        std::fs::write(&existing, b"existing").unwrap();
        let err =
            super::install_downloaded_sidecars(dir.path(), b"not a raven db".to_vec()).unwrap_err();
        assert!(err.contains("names.db"));
        assert!(err.contains("RAVEN_NAMES_DB"), "got {err}");
        assert_eq!(std::fs::read(&existing).unwrap(), b"existing");
    }

    #[test]
    fn atomic_install_round_trips_single_names_db() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let recs = vec![PackageRecord {
            name: "dplyr".into(),
            version: "1.1.4".into(),
            exports: vec!["mutate".into()],
            depends: vec![],
            lazy_data: vec![],
        }];
        let prov = ShippedDbProvenance {
            source: "t".into(),
            snapshot_date: "2026-06-01".into(),
            package_count: 1,
            raven_version: "9.9.9".into(),
        };
        let names_src = source.path().join("names.db");
        write_shipped_db(&names_src, &recs, prov).unwrap();

        let installed =
            super::install_downloaded_sidecars(dest.path(), std::fs::read(&names_src).unwrap())
                .unwrap();
        assert_eq!(installed.names_db_path, dest.path().join("names.db"));
        ShippedDb::open(&installed.names_db_path).unwrap();
    }

    #[test]
    fn download_asset_blocking_rejects_non_http_urls() {
        let err = super::download_asset_blocking("file:///tmp/names.db").unwrap_err();
        assert!(err.contains("http://"), "got {err}");
        assert!(err.contains("https://"), "got {err}");
        assert!(err.contains("RAVEN_NAMES_DB"), "got {err}");
    }

    #[test]
    fn curl_failure_errors_suggest_manual_sidecars() {
        let err = super::curl_failure_error(
            "https://example.invalid/names.db",
            "exit status: 7",
            "could not connect",
        );
        assert!(err.contains("RAVEN_NAMES_DB"), "got {err}");
    }

    #[test]
    fn download_asset_blocking_rejects_ftp_without_network() {
        let err = super::download_asset_blocking("ftp://example.invalid/file").unwrap_err();
        assert!(err.contains("http://"), "got {err}");
        assert!(err.contains("https://"), "got {err}");
        assert!(err.contains("RAVEN_NAMES_DB"), "got {err}");
    }

    #[cfg(not(windows))]
    #[test]
    fn replace_with_tmp_preserves_existing_final_when_tmp_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing_tmp = dir.path().join("missing.tmp");
        let final_path = dir.path().join("names.db");
        std::fs::write(&final_path, b"existing").unwrap();

        let err = super::replace_with_tmp(&missing_tmp, &final_path).unwrap_err();

        assert!(err.contains("replace"), "got {err}");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
    }

    #[test]
    fn collect_used_package_names_unions_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // R file referencing ggplot2 and dplyr
        std::fs::write(
            root.join("script.R"),
            "library(ggplot2)\ndplyr::mutate(x)\n",
        )
        .unwrap();

        // DESCRIPTION with Imports: cli
        std::fs::write(root.join("DESCRIPTION"), "Package: mypkg\nImports: cli\n").unwrap();

        // renv.lock pinning tibble
        std::fs::write(
            root.join("renv.lock"),
            r#"{"Packages":{"tibble":{"Package":"tibble","Version":"3.2.1"}}}"#,
        )
        .unwrap();

        let names = super::collect_used_package_names(root).unwrap();
        let set: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
        assert!(set.contains("ggplot2"), "missing ggplot2, got {set:?}");
        assert!(set.contains("dplyr"), "missing dplyr, got {set:?}");
        assert!(set.contains("cli"), "missing cli, got {set:?}");
        assert!(set.contains("tibble"), "missing tibble, got {set:?}");
    }

    #[test]
    fn parse_fetch_args_defaults() {
        let args = super::parse_fetch_args(std::iter::empty()).unwrap();
        assert!(!args.missing_only);
        assert!(!args.fail_on_missing);
        assert_eq!(
            args.output,
            std::path::PathBuf::from(".raven/packages.json")
        );
        assert_eq!(args.workspace, None);
        assert_eq!(
            args.base_urls,
            vec![
                "https://cran.r-universe.dev".to_string(),
                "https://bioc.r-universe.dev".to_string(),
            ]
        );
    }

    #[test]
    fn parse_fetch_args_all_flags() {
        let args = super::parse_fetch_args(
            [
                "--missing-only",
                "--fail-on-missing",
                "--output",
                "x.json",
                "--workspace",
                "w",
                "--base-urls",
                "http://a,http://b",
            ]
            .iter()
            .map(|s| s.to_string()),
        )
        .unwrap();
        assert!(args.missing_only);
        assert!(args.fail_on_missing);
        assert_eq!(args.output, std::path::PathBuf::from("x.json"));
        assert_eq!(args.workspace, Some(std::path::PathBuf::from("w")));
        assert_eq!(
            args.base_urls,
            vec!["http://a".to_string(), "http://b".to_string()]
        );
    }

    #[test]
    fn parse_fetch_args_help() {
        let err = super::parse_fetch_args(["--help"].iter().map(|s| s.to_string())).unwrap_err();
        assert_eq!(err, "HELP");
    }

    #[test]
    fn parse_fetch_args_unknown_flag() {
        let err = super::parse_fetch_args(["--bogus"].iter().map(|s| s.to_string())).unwrap_err();
        assert!(err.contains("unknown flag"), "got {err}");
        assert!(err.contains("--bogus"), "got {err}");
    }

    #[test]
    fn parse_fetch_args_rejects_empty_base_urls() {
        // `--base-urls ''` (or only commas/whitespace) parses to no URLs; reject
        // it with a clear message rather than later misreporting a network outage.
        let err = super::parse_fetch_args(["--base-urls", " , "].iter().map(|s| s.to_string()))
            .unwrap_err();
        assert!(err.contains("--base-urls"), "got {err}");
    }

    #[test]
    fn embedded_base_packages_contains_expected() {
        let set = super::embedded_base_packages();
        // Attached-7 plus a non-attached base-priority package: all 14 are the
        // embedded/R-free base set used by fetch and names.db generation.
        // `freeze` intentionally uses PackageLibrary::is_base_package instead,
        // so explicit local-R packages like grid can still be recorded.
        for pkg in ["base", "stats", "utils", "methods", "datasets", "grid"] {
            assert!(set.contains(pkg), "expected {pkg} in base set");
        }
        assert!(!set.contains("dplyr"), "dplyr must not be in base set");
    }

    // --- Test server for fetch tests ---

    /// Minimal blocking HTTP server for fetch tests. Serves fixture JSON for
    /// known packages, 404 for unknown ones.
    struct TestServer {
        addr: std::net::SocketAddr,
        _handle: std::thread::JoinHandle<()>,
    }

    /// How the test server responds to every request.
    #[derive(Clone, Copy)]
    enum ServerMode {
        /// Serve the fixture for known packages, 404 for unknown ones.
        Fixtures,
        /// Always 404 (used to exercise the CRAN→Bioc fallback).
        NotFound,
        /// Always 500 (used to prove a 5xx outage is a hard Transport error,
        /// not a soft NotFound that silently drops a package).
        ServerError,
        /// Return 500 for dplyr, but 404 for every other package.
        DplyrServerErrorOtherwiseNotFound,
        /// Serve a package whose Depends includes cli; every other package is 404.
        RootDependsCli,
    }

    impl TestServer {
        /// Start a test server. If `always_404` is true, every request gets 404.
        fn start(always_404: bool) -> Self {
            Self::start_mode(if always_404 {
                ServerMode::NotFound
            } else {
                ServerMode::Fixtures
            })
        }

        fn start_mode(mode: ServerMode) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    std::thread::spawn(move || Self::handle_conn(stream, mode));
                }
            });
            TestServer {
                addr,
                _handle: handle,
            }
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.addr.port())
        }

        fn handle_conn(mut stream: std::net::TcpStream, mode: ServerMode) {
            use std::io::{BufRead, BufReader, Write};
            let reader = BufReader::new(&stream);
            let request_line = match reader.lines().next() {
                Some(Ok(line)) => line,
                _ => return,
            };
            // Parse: "GET /api/packages/dplyr HTTP/1.1"
            let parts: Vec<&str> = request_line.split_whitespace().collect();
            if parts.len() < 2 {
                return;
            }
            let path = parts[1];

            let not_found =
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            match mode {
                ServerMode::NotFound => {
                    let _ = write!(stream, "{not_found}");
                    return;
                }
                ServerMode::ServerError => {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                ServerMode::DplyrServerErrorOtherwiseNotFound => {
                    if path == "/api/packages/dplyr" {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                    } else {
                        let _ = write!(stream, "{not_found}");
                    }
                    return;
                }
                ServerMode::RootDependsCli => {
                    if path == "/api/packages/rootpkg" {
                        let body = r#"{
  "Package": "rootpkg",
  "Version": "1.0.0",
  "_exports": ["root_fn"],
  "_dependencies": [
    { "package": "cli", "version": "*", "role": "Depends" }
  ],
  "_datasets": []
}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    } else {
                        let _ = write!(stream, "{not_found}");
                    }
                    return;
                }
                ServerMode::Fixtures => {}
            }

            let prefix = "/api/packages/";
            if let Some(pkg) = path.strip_prefix(prefix) {
                let fixture_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/package_db/runiverse");
                let fixture_path = fixture_dir.join(format!("{pkg}.json"));
                if let Ok(body) = std::fs::read(&fixture_path) {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                } else {
                    let _ = write!(stream, "{not_found}");
                }
            } else {
                let _ = write!(stream, "{not_found}");
            }
        }
    }

    #[test]
    fn fetch_one_package_found() {
        let server = TestServer::start(false);
        let bases = vec![server.base_url()];
        let result = super::fetch_one_package_blocking("dplyr", &bases);
        match result {
            super::PackageFetchOutcome::Found(rec) => {
                assert_eq!(rec.name, "dplyr");
                assert_eq!(rec.version, "1.1.4");
                assert!(
                    rec.exports.contains(&"mutate".to_string()),
                    "{:?}",
                    rec.exports
                );
                assert!(
                    rec.exports.contains(&"filter".to_string()),
                    "{:?}",
                    rec.exports
                );
                assert!(
                    rec.exports.contains(&"select".to_string()),
                    "{:?}",
                    rec.exports
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn fetch_one_package_cran_miss_bioc_fallback() {
        let server_404 = TestServer::start(true);
        let server_ok = TestServer::start(false);
        let bases = vec![server_404.base_url(), server_ok.base_url()];
        let result = super::fetch_one_package_blocking("dplyr", &bases);
        match result {
            super::PackageFetchOutcome::Found(rec) => {
                assert_eq!(rec.name, "dplyr");
                assert_eq!(rec.version, "1.1.4");
            }
            other => panic!("expected Found from second base, got {other:?}"),
        }
    }

    #[test]
    fn fetch_one_package_not_found_on_all_bases() {
        let server = TestServer::start(false);
        let bases = vec![server.base_url()];
        let result = super::fetch_one_package_blocking("nonexistent_pkg_xyz", &bases);
        assert!(
            matches!(result, super::PackageFetchOutcome::NotFound),
            "expected NotFound, got {result:?}"
        );
    }

    #[test]
    fn fetch_one_package_transport_when_unreachable() {
        // Bind to get a free port, then drop the listener so nothing is listening.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let bases = vec![format!("http://127.0.0.1:{}", addr.port())];
        let result = super::fetch_one_package_blocking("dplyr", &bases);
        assert!(
            matches!(result, super::PackageFetchOutcome::Transport(_)),
            "expected Transport, got {result:?}"
        );
    }

    #[test]
    fn curl_http_error_distinguishes_not_found_from_server_error() {
        // curl --fail emits "curl: (22) The requested URL returned error: NNN".
        // Only 404/410 are a genuine not-found (soft); a 5xx outage must NOT be.
        assert!(super::curl_http_error_is_not_found(
            "curl: (22) The requested URL returned error: 404"
        ));
        assert!(super::curl_http_error_is_not_found(
            "curl: (22) The requested URL returned error: 410 Gone"
        ));
        assert!(!super::curl_http_error_is_not_found(
            "curl: (22) The requested URL returned error: 500"
        ));
        assert!(!super::curl_http_error_is_not_found(
            "curl: (22) The requested URL returned error: 503 Service Unavailable"
        ));
        // Unparseable stderr conservatively returns false (treated as Transport).
        assert!(!super::curl_http_error_is_not_found(
            "curl: (22) something odd"
        ));
    }

    #[test]
    fn fetch_one_package_server_error_is_transport_not_not_found() {
        // A 5xx from every base is a transient outage, not a missing package:
        // it must be Transport (hard) so run_fetch can refuse rather than write
        // a database that silently drops the package.
        let server = TestServer::start_mode(ServerMode::ServerError);
        let bases = vec![server.base_url()];
        let result = super::fetch_one_package_blocking("dplyr", &bases);
        assert!(
            matches!(result, super::PackageFetchOutcome::Transport(_)),
            "expected Transport for a 5xx, got {result:?}"
        );
    }

    #[test]
    fn fetch_one_package_5xx_on_one_base_not_masked_by_404_on_another() {
        // A 5xx outage on the first base must NOT be masked by a genuine 404 on
        // the second: the package might exist on the outaged host, so the result
        // is Transport (hard), not NotFound (soft) — otherwise a partial outage
        // could silently drop a real package.
        let outage = TestServer::start_mode(ServerMode::ServerError);
        let not_found = TestServer::start_mode(ServerMode::NotFound);
        let bases = vec![outage.base_url(), not_found.base_url()];
        let result = super::fetch_one_package_blocking("dplyr", &bases);
        assert!(
            matches!(result, super::PackageFetchOutcome::Transport(_)),
            "expected Transport when one base 5xx'd, got {result:?}"
        );
    }

    #[tokio::test]
    async fn fetch_packages_wave_fetches_all() {
        let server = TestServer::start(false);
        let bases = vec![server.base_url()];
        let names = vec![
            "dplyr".to_string(),
            "ggplot2".to_string(),
            "bogus_nonexistent".to_string(),
        ];
        let results = super::fetch_packages_wave(names, &bases).await;
        let map: std::collections::HashMap<&str, &super::PackageFetchOutcome> =
            results.iter().map(|(n, o)| (n.as_str(), o)).collect();

        assert!(matches!(map["dplyr"], super::PackageFetchOutcome::Found(r) if r.name == "dplyr"));
        assert!(
            matches!(map["ggplot2"], super::PackageFetchOutcome::Found(r) if r.name == "ggplot2")
        );
        assert!(matches!(
            map["bogus_nonexistent"],
            super::PackageFetchOutcome::NotFound
        ));
    }

    // --- Task 3: run_fetch tests ---

    #[test]
    fn renv_skew_warnings_flags_only_mismatches() {
        let fetched = vec![
            PackageRecord {
                name: "dplyr".into(),
                version: "1.1.4".into(),
                exports: vec![],
                depends: vec![],
                lazy_data: vec![],
            },
            PackageRecord {
                name: "ggplot2".into(),
                version: "3.5.0".into(),
                exports: vec![],
                depends: vec![],
                lazy_data: vec![],
            },
        ];
        let mut pinned = std::collections::HashMap::new();
        pinned.insert("dplyr".to_string(), "1.1.2".to_string());
        pinned.insert("ggplot2".to_string(), "3.5.0".to_string());

        let warnings = super::renv_skew_warnings(&fetched, &pinned);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("dplyr"), "got: {}", warnings[0]);
        assert!(warnings[0].contains("1.1.4"), "got: {}", warnings[0]);
        assert!(warnings[0].contains("1.1.2"), "got: {}", warnings[0]);

        // Empty pinned => no warnings
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let warnings2 = super::renv_skew_warnings(&fetched, &empty);
        assert!(warnings2.is_empty());
    }

    #[tokio::test]
    async fn run_fetch_writes_used_packages() {
        let server = TestServer::start(false);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\n").unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(result.is_ok(), "run_fetch failed: {:?}", result);

        let out = root.join(".raven/packages.json");
        let db = read_repo_db_file(&out).unwrap();
        let dplyr = db.packages.iter().find(|r| r.name == "dplyr").unwrap();
        assert!(dplyr.exports.contains(&"mutate".to_string()));
        assert!(dplyr.exports.contains(&"filter".to_string()));
        assert!(dplyr.exports.contains(&"select".to_string()));
        assert_eq!(db.provenance.r_version, "none (fetched)");
    }

    #[tokio::test]
    async fn run_fetch_merge_existing_wins() {
        let server = TestServer::start(false);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\nlibrary(ggplot2)\n").unwrap();

        // Pre-create with a sentinel dplyr record
        let out = root.join(".raven/packages.json");
        let sentinel_db = RepoDb {
            schema_version: REPO_DB_SCHEMA_VERSION,
            provenance: RepoDbProvenance {
                raven_version: "0.0.0".into(),
                r_version: "test".into(),
                generated_unix: 0,
            },
            packages: vec![PackageRecord {
                name: "dplyr".into(),
                version: "0.0.0".into(),
                exports: vec!["OLD_SENTINEL".into()],
                depends: vec![],
                lazy_data: vec![],
            }],
        };
        write_repo_db_file(&out, &sentinel_db).unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(result.is_ok(), "run_fetch failed: {:?}", result);

        let db = read_repo_db_file(&out).unwrap();
        let dplyr = db.packages.iter().find(|r| r.name == "dplyr").unwrap();
        assert_eq!(dplyr.version, "0.0.0");
        assert_eq!(dplyr.exports, vec!["OLD_SENTINEL".to_string()]);
        let ggplot2 = db.packages.iter().find(|r| r.name == "ggplot2").unwrap();
        assert_eq!(ggplot2.version, "3.5.0");
    }

    #[tokio::test]
    async fn run_fetch_noop_when_fully_covered() {
        let server = TestServer::start(false);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\n").unwrap();

        let out = root.join(".raven/packages.json");
        let existing_db = RepoDb {
            schema_version: REPO_DB_SCHEMA_VERSION,
            provenance: RepoDbProvenance {
                raven_version: "0.0.0".into(),
                r_version: "test".into(),
                generated_unix: 0,
            },
            packages: vec![PackageRecord {
                name: "dplyr".into(),
                version: "1.1.4".into(),
                exports: vec!["filter".into(), "mutate".into(), "select".into()],
                depends: vec!["R".into(), "cli".into()],
                lazy_data: vec![],
            }],
        };
        write_repo_db_file(&out, &existing_db).unwrap();
        let bytes_before = std::fs::read(&out).unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(result.is_ok(), "run_fetch failed: {:?}", result);

        let bytes_after = std::fs::read(&out).unwrap();
        assert_eq!(
            bytes_before, bytes_after,
            "file should be unchanged (no-op)"
        );
    }

    #[tokio::test]
    async fn run_fetch_resolved_nowhere_exit_codes() {
        let server = TestServer::start(false);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(boguspkgxyz)\n").unwrap();

        // Without --fail-on-missing => Ok
        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(
            result.is_ok(),
            "expected Ok without --fail-on-missing, got: {:?}",
            result
        );

        // With --fail-on-missing => Err
        // Remove the output so it starts fresh
        let _ = std::fs::remove_file(root.join(".raven/packages.json"));
        let args2 = super::FetchArgs {
            missing_only: false,
            fail_on_missing: true,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result2 = super::run_fetch(args2).await;
        assert!(result2.is_err(), "expected Err with --fail-on-missing");
        let err = result2.unwrap_err();
        assert!(err.contains("could not be resolved"), "got: {err}");
        assert!(err.contains("--fail-on-missing"), "got: {err}");
    }

    #[tokio::test]
    async fn run_fetch_fail_on_missing_includes_transitive_depends() {
        let server = TestServer::start_mode(ServerMode::RootDependsCli);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(rootpkg)\n").unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: true,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(
            result.is_err(),
            "expected --fail-on-missing to fail when rootpkg's cli dependency is unresolved"
        );
        let err = result.unwrap_err();
        assert!(err.contains("could not be resolved"), "got: {err}");
        assert!(err.contains("--fail-on-missing"), "got: {err}");
    }

    #[tokio::test]
    async fn run_fetch_missing_only_degrades_without_r() {
        // When --missing-only is set and R is NOT available, the tier1 lib
        // degrades (package_exists returns false for everything), so the fetch
        // proceeds as if --missing-only were not set. On machines WITH R where
        // dplyr IS installed, the package is correctly subtracted. Either way
        // run_fetch must succeed.
        let server = TestServer::start(false);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\n").unwrap();

        let args = super::FetchArgs {
            missing_only: true,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(result.is_ok(), "run_fetch failed: {:?}", result);

        let out = root.join(".raven/packages.json");
        // If R is available and dplyr is installed, it's subtracted (no file written).
        // If R is unavailable, dplyr is fetched (file written with dplyr).
        // Both are correct behavior.
        if out.exists() {
            let db = read_repo_db_file(&out).unwrap();
            assert!(
                db.packages.iter().any(|r| r.name == "dplyr"),
                "if file written, dplyr should be present"
            );
        }
        // The key assertion: no error regardless of R availability.
    }

    #[tokio::test]
    async fn run_fetch_all_transport_is_hard_error() {
        // Bind to get a free port, then drop so nothing listens
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\n").unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![format!("http://127.0.0.1:{}", addr.port())],
        };
        let result = super::run_fetch(args).await;
        assert!(
            result.is_err(),
            "expected hard error on total transport failure"
        );
        let err = result.unwrap_err();
        assert!(err.contains("could not reach"), "got: {err}");

        // No output file created
        assert!(!root.join(".raven/packages.json").exists());
    }

    #[tokio::test]
    async fn run_fetch_mixed_notfound_and_transport_with_no_records_is_hard_error() {
        let server = TestServer::start_mode(ServerMode::DplyrServerErrorOtherwiseNotFound);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("script.R"),
            "library(boguspkgxyz)\nlibrary(dplyr)\n",
        )
        .unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(
            result.is_err(),
            "expected hard error when no records were fetched and any request hit transport failure"
        );
        let err = result.unwrap_err();
        assert!(err.contains("could not reach"), "got: {err}");
        assert!(
            !root.join(".raven/packages.json").exists(),
            "no file should be written when zero records were fetched during a mixed outage"
        );
    }

    #[tokio::test]
    async fn run_fetch_server_outage_5xx_is_hard_error_not_silent_write() {
        // A 5xx from every host (transient r-universe outage) must hard-error
        // and write nothing — NOT silently write a database that drops the
        // package as if it were "resolved nowhere".
        let server = TestServer::start_mode(ServerMode::ServerError);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\n").unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(result.is_err(), "expected hard error on a 5xx outage");
        assert!(
            !root.join(".raven/packages.json").exists(),
            "no file should be written during a 5xx outage"
        );
    }

    #[tokio::test]
    async fn run_fetch_refuses_corrupt_existing() {
        let server = TestServer::start(false);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("script.R"), "library(dplyr)\n").unwrap();

        let out = root.join(".raven/packages.json");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        std::fs::write(&out, "{ not valid json").unwrap();

        let args = super::FetchArgs {
            missing_only: false,
            fail_on_missing: false,
            output: std::path::PathBuf::from(".raven/packages.json"),
            workspace: Some(root.to_path_buf()),
            base_urls: vec![server.base_url()],
        };
        let result = super::run_fetch(args).await;
        assert!(result.is_err(), "expected error on corrupt existing file");
        let err = result.unwrap_err();
        assert!(err.contains("unreadable"), "got: {err}");

        // Corrupt file left intact
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "{ not valid json");
    }

    #[test]
    fn emit_embedded_base_source_escapes_and_separates() {
        let pkgs = vec![
            (
                "base".to_string(),
                vec!["c".to_string(), "%in%".to_string()],
                Vec::<String>::new(),
                Vec::<String>::new(),
            ),
            (
                "datasets".to_string(),
                Vec::<String>::new(),
                vec!["mtcars".to_string()],
                Vec::<String>::new(),
            ),
        ];
        let src = super::emit_embedded_base_source(&pkgs, "R 4.4.0");
        assert!(src.contains("// @generated"));
        assert!(src.contains("Reference R version: R 4.4.0"));
        assert!(
            src.contains(r#""%in%""#),
            "operator export emitted as a string literal"
        );
        assert!(src.contains(r#"name: "datasets""#));
        assert!(src.contains(r#"datasets: &["mtcars"]"#));
    }
}
