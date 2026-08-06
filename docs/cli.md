# CLI

Raven ships a single binary that serves the LSP via stdio *and* exposes subcommands for use outside an editor. To check your code, run either:

- `raven check` — index a workspace and report the **full** diagnostic set (the same diagnostics the editor publishes).
- `raven lint` — run the native **style** linter only.

Run `raven`, `raven --help`, or `raven help` for top-level usage. Commands with options accept `--help`, including package-database commands such as `raven packages fetch --help` and the top-level aliases `raven fetch --help` / `raven freeze --help`.

If your codebase imports R packages, Raven needs to know the symbols those packages export. On a machine with R and the project's packages installed, `raven check` reads that metadata directly from your R installation. In CI, where restoring the full package library can dominate the runtime of a static check, use the package database commands below instead.

- `raven packages freeze` — generate a committed `.raven/packages.json` package symbol database. This is the reproducible, project-specific path for CI: it captures the exports your installed project dependencies expose and should be committed when you need version-pinned package metadata. Choose its scope with `--used` (default — only the packages the repo uses) or `--installed`/`--all` (every installed package). See [`raven packages freeze`](#raven-packages-freeze) and [Package database](package-database.md).
- `raven packages fetch` — produce the same `.raven/packages.json` from CRAN/Bioconductor r-universe instead of a local R install. Needs no R, no installed packages, and no dependency on the `names-db` Release. See [`raven packages fetch`](#raven-packages-fetch) and [Package database](package-database.md).
- `raven packages update` — download the CRAN/Bioconductor metadata Raven uses to recognize symbols from packages that are not installed. No install bundles this database, so run it whenever you want broad coverage without a local R install. See [`raven packages update`](#raven-packages-update).

If you are new to CI, start with [Automated checks in CI](ci.md). It explains what CI is, how Raven checks R code without running the analysis, and gives copyable GitHub Actions and Bitbucket Pipelines files.

## Why Raven analyzes R without running it

Most language ecosystems have a CI checker that reads code without executing it — `cargo check`, `tsc`, `pyright`, `ruff`. R's tooling grew up around a different need: `R CMD check` and the CI ecosystem around it verify *packages*, which means installing every dependency and running code. There's little equivalent for *analysis repositories* — the scripts that make up most scientific and statistical work — and that gap is what these commands fill.

Raven's core is static, semantic analysis with **position-aware scope**: it tracks what's defined at each point in a script — line by line, and inside each function — so it can flag an undefined variable, including one used before it's defined. Crucially for analysis repos, it follows `source()` chains and static `targets::tar_source()` batches, building a map of how your scripts depend on one another and resolving scope across them. No other R tool does this, and it's what makes `raven check` useful both for package development and for the analysis repositories the rest of the R tooling largely overlooks.

If you aren't building a package, you usually don't want CI to *run* your scripts — and you certainly don't want to install every package they depend on just to check them. Raven finishes in about a second; compiling a dependency tree can take hours, which makes the check slow and expensive. At scale it would also push avoidable load onto CRAN mirrors and the wider ecosystem. So Raven runs **without R, and without your repo's packages installed**.

It still needs each package's **export names** to tell a real symbol from a typo. There are two easy ways to supply them:

- **`raven packages update` before `raven check`** (easiest) — downloads a package symbol database from Raven's GitHub repository. It is seeded from the maintainer's system and refreshed every Monday with all of CRAN and Bioconductor, retrieved from [r-universe](https://r-universe.dev).
- **Commit `raven packages freeze`** (simplest if you'd rather not depend on Raven's database) — records the exports of just the packages your repo uses, read from your installed packages (via renv and/or your machine), into `.raven/packages.json`. Commit that file and CI needs only `raven check`.

And if R *is* installed in CI with all the packages available, none of this is needed: Raven reads everything it needs straight from that R installation. The [CI package metadata strategies](#package-metadata-strategies) below lay out the trade-offs in detail.

## `raven check`

Index the workspace, then report the full diagnostic set for the requested files and exit with a code suitable for CI gating.

```text
raven check [OPTIONS] [PATHS...]
```

Diagnostics reported (subject to configured severities — see [diagnostics.md](diagnostics.md)):

- R syntax errors and semantic checks (e.g. assignment-in-condition, mixed logical operators). Standalone JAGS/BUGS and Stan diagnostics are available when opted in through `[diagnostics] jags = "on"` and/or `stan = "on"` in `raven.toml`.
- The native style lints (when enabled via `raven.toml` / `.lintr`).
- Cross-file diagnostics: missing sourced files, circular dependencies, exceeded max source-chain depth, redundant directives, out-of-scope usage, and case-only path mismatches (in `source()` calls, forward directives, and backward `# raven: sourced-by`-style directives). On a case-sensitive CI filesystem a case-only mismatch (e.g. `source("scripts/templates.r")` for an on-disk `templates.R`) is a **warning**, so it exceeds the default `--max-severity info` and fails the build — surfacing a portability bug that, for a forward `source()`, would also break it at runtime on Linux. See [Diagnostics → Source path case mismatch](diagnostics.md#source-path-case-mismatch).
- Static `{box}` and `{import}` selective-import scope, missing-module, package, and authoritative missing-export diagnostics use the same analysis as the language server.
- Missing-package warnings (`library(notInstalled)`) — see [Missing-package reporting in CI](#missing-package-reporting-in-ci); `raven check` suppresses these by default.
- Undefined-variable diagnostics, accounting for cross-file and package scope.

### Workspace and paths

The workspace is indexed, except for paths matched by `[workspace].exclude`, so cross-file resolution is accurate for included files. The workspace root is `--workspace DIR`, defaulting to the current directory. `PATHS` only filter **which files have their diagnostics reported**:

- With no `PATHS`, every included R file and standalone `.jags`, `.bugs`, `.bug`, or `.stan` program in the workspace is reported.
- With `PATHS`, explicit files are reported as named, while directories are walked recursively for included R files and standalone `.jags`, `.bugs`, `.bug`, or `.stan` programs. Extension matching is case-insensitive. `.stanfunctions` files are excluded because they are include fragments rather than standalone programs. Indexing still covers the included workspace, so a reported R file's `source()` targets resolve even when they aren't named.

Model files remain discoverable targets even though their diagnostics default
to off. Enable `[diagnostics] jags = "on"` for syntax-only JAGS/BUGS
checking and `[diagnostics] stan = "on"` for Stan syntax plus conservative
undeclared-variable checking. Stan semantic checking runs when the file contains
at least one real top-level program block. A file with no real block—including
an assembler fragment organized with `//--- data` or similar section
comments—gets syntax diagnostics only. Missing one optional block does not
suppress checking, so values supplied by R still require a Stan `data`
declaration. Any `#include` suppresses Stan semantic findings for the whole file
because Raven does not preprocess or resolve includes. Setting
`undefinedVariableSeverity = "off"` suppresses only Stan semantic findings while
leaving opted-in syntax diagnostics active.

Raven does not run JAGS or `stanc`, validate dimensions, types, or distribution
signatures, or apply R lint, package, or cross-file analysis to model source.
Stan and JAGS share the `[diagnostics]`
`maxSyntaxDiagnosticsPerFile` `raven.toml` setting (default `500`,
`0` for unlimited); it is applied after exact deduplication and stable source
ordering in `text`, `json`, and `sarif` output. The three JAGS suffixes select
strict JAGS-dialect validation, not general OpenBUGS, WinBUGS, MultiBUGS, or
NIMBLE compatibility. Stan undeclared-variable findings use a separate fixed
500-per-file semantic bound and the configured undefined-variable severity; the
syntax cap does not affect them.

`raven.toml` can exclude generated or vendored trees from discovery:

```toml
[workspace]
exclude = ["generated/**", "archive/**", "!generated/keep.R"]
```

`[workspace].exclude` defaults to `[]`. Patterns are project-root-relative globs, matched against paths relative to the containing workspace root. `*` and `?` do not cross `/`; use `**` for recursive matches, such as `**/*.log` for `.log` files at any depth. They apply to the workspace index and to default `raven check` discovery. Directory globs like `generated/**` prune whole directories before the parallel parse/index phase; when any negated re-include pattern (`!pattern`) is present, Raven disables directory pruning for the list so a re-included file is never skipped early. The full ordered matcher still filters files, with the last matching pattern winning.

Explicit file arguments bypass these exclusions: `raven check generated/one.R` still reports `generated/one.R` even if `generated/**` is excluded. Directory arguments are discovery walks, so exclusions apply inside them.

Static `targets::tar_source()` batches retain their ordered execution context
during CLI analysis, including when the same script is selected more than once
with different working-directory behavior. `raven check` supplements the
ordinary URI-based dependency neighborhood with a bounded, context-sensitive
provider traversal; providers outside the initial workspace scan are parsed
and finalized into the same workspace index before diagnostics run. This
affects scope only: navigation, missing-file checks, and dependency
revalidation continue to use one URI-global graph edge per source entry.

### Options

- `--workspace DIR` — workspace root to index (default: current directory).
- `--config PATH` — explicit path to a `raven.toml` or `.lintr` (default: walk upward from the workspace root, discovering a `raven.toml` or non-home `.lintr`; literal `~/.lintr` is not auto-discovered, but can be used with `--config ~/.lintr`).
- `--no-config` — ignore `raven.toml` and `.lintr`; use Raven's built-in defaults. JAGS and Stan diagnostics remain disabled because both model-language defaults are `"off"`.
- `--format text|json|sarif` — default `text`.
- `--max-severity off|hint|info|warning|error` — highest severity that does **not** fail the build (default `info`). With the built-in defaults, undefined-variable and missing-file diagnostics are `warning` and circular dependencies are `error`, so they fail the build at the default threshold. Native style lints default to `information` (below `warning`), so they pass at `info` but gate at `--max-severity hint` or `off`.
- `--report-uninstalled` (see [Missing-package reporting in CI](#missing-package-reporting-in-ci)) — re-enable missing-package warnings, which `raven check` otherwise suppresses by default.
- `--quiet` — suppress the trailing summary line.
- `--color auto|always|never` — when to colorize `text` output (default `auto`). See [Color output](#color-output).
- `--no-color` — alias for `--color never`.

### R and packages

`raven check` auto-detects R on `PATH` to resolve installed-package exports and exact local metadata (it runs `.libPaths()` and parses package `NAMESPACE` files, the same as the language server). It honors the same `raven.toml` package settings the editor does: `packages.enabled = false` disables R detection entirely (no R subprocess and no installed/local package diagnostics — matching the editor), `packages.rPath` selects the R binary instead of `PATH` auto-detection, and `packages.additionalLibraryPaths` adds extra library directories to the search path. If R is not found, `library()` calls aren't checked against installed packages, but base R-platform symbols have embedded coverage in the binary. Broad CRAN/Bioconductor coverage without R comes from Raven's `names.db` database, installed with `raven packages update`. A one-line note is printed to stderr when R is absent. All other diagnostics still run.

Before reporting, `raven check` warms the export cache for the packages each reported file attaches with `library()` / `require()` or a static qualified `pacman::p_load()`, and permissively prefetches statically known targets of a bare `p_load()` so graph-aware analysis can decide whether `{pacman}` is attached at that call position. Bare calls contribute visible/reportable packages only when that prerequisite is satisfied; locally shadowed calls and dynamic package arguments contribute no package metadata. Warming also covers calls in R Markdown/Quarto chunks, follows static `{box}` / `{import}` local-module chains from both plain R and chunk targets, and includes packages contributed by the workspace `.Rprofile` or test preambles and, in [R package workspaces](r-package-dev.md), packages the `NAMESPACE` fully imports with `import(pkg)`. This makes a bare call into an attached package that isn't one of its exports fail the same way in the CLI and editor, while full-import exports resolve in both call and value position. Workspace and prelude discovery follow explicit `source()` relationships in execution order when deciding whether an inherited `pacman` attachment enables bare `p_load()`. One narrow gap remains: a package attached only *indirectly* in an ordinary reported file's `source()` closure is not always pre-warmed, so calls that could resolve to it are left unflagged rather than risk a false positive. Attach the package directly in the file (or rely on the editor) if you need those calls checked.

### Missing-package reporting in CI

`raven check` resolves package **export names** through an ordered three-tier fallback — installed packages, then a committed `.raven/packages.json`, then Raven's broad CRAN/Bioconductor metadata when available — so symbols from attached packages can resolve even when no R is installed. That metadata is downloaded with `raven packages update` — it isn't bundled with the binary — so a CI image that installs Raven needs that step for broad CRAN/Bioconductor coverage; embedded base R-platform coverage is in the binary regardless. This stops the undefined-variable storm that otherwise makes Raven unusable in CI. See [Package database](package-database.md).

Knowing a package's exports is **separate** from knowing whether it is installed. The **missing-package** diagnostic answers a different question — *"will `library(X)` succeed at runtime?"*, i.e. is `X` installed? — so it is driven solely by what is present in the local library paths, never by the package symbol database. Because CI deliberately omits package installation, `raven check` **suppresses generic missing-package warnings by default**.

`--report-uninstalled` re-enables them. Reach for it whenever a `library(X)` call must really succeed at runtime: a pipeline that *does* install packages (e.g. `renv::restore()`) and wants to fail if any didn't, **or** any CI that **actually runs your R scripts** after `raven check` (e.g. R-package CI), where an uninstalled package is a genuine failure rather than CI noise. Gate-only CI that never executes the scripts wants the default (suppressed). It reports `library()` calls **not present in the local library paths** — **not** relative to the package symbol databases (`.raven/packages.json` or the `names.db` database). One consequence: with the default off, a genuine typo such as `library(dpylr)` is silent (no database knows it, but `raven check` isn't checking install status); it is reported only with `--report-uninstalled`. The language server is unchanged — it still fires missing-package whenever install state is known.

One actionable subtype remains enabled even without the flag:
`package-outside-active-library` means Raven successfully activated the
workspace's renv project, the referenced package is unavailable in its active
library, and a validated package with that name exists in a vanilla system/user
library removed by activation. That is evidence of a project setup problem, not
the ordinary "CI did not install dependencies" case; run `renv::hydrate()` to
add installed packages to the project library. See
[Diagnostics](diagnostics.md#package-names-vs-install-status) for the full
model.

### File encoding

Source files must be UTF-8. A UTF-8 byte-order mark is stripped and BOM-marked UTF-16 (LE/BE) is decoded, but anything else must already be valid UTF-8 — Raven does not guess legacy single-byte encodings (Latin-1 / Windows-1252). Guessing would silently mis-decode: a non-breaking space (`0xA0`) inside a string comparison, for instance, would read as an ordinary space and quietly change what your code matches. A *reported* file that isn't valid UTF-8 is therefore flagged as an **error diagnostic** (`File is not valid UTF-8: first invalid byte 0x… at offset …`) that fails the build like any other error finding — it is **not** an operator error. Re-save the file as UTF-8 to fix it. A file that is only *indexed* for cross-file resolution (not itself reported) and can't be decoded is silently skipped, matching the editor. `raven lint` reads through the same decoder, so encoding handling is uniform across the user-facing CLI.

### Exit codes

- `0` — no diagnostic exceeded `--max-severity`.
- `1` — at least one diagnostic exceeded `--max-severity`. An unknown flag is a usage error and also exits `1`. A reported file that isn't valid UTF-8 is an error diagnostic, so it also exits `1`.
- `2` — operator error detected while running (config parse failure, an I/O failure reading a path, invalid workspace). A *readable* but mis-encoded file is a finding (exit `1`), not an operator error.

## CI examples

Use `raven check` in CI as a static gate for pull requests and pushes. If you want a beginner-friendly overview before the command reference, see [Automated checks in CI](ci.md). The important setup choice is package metadata: Raven does not need to execute your R code, and CI usually does not need to install every package just to resolve exports. Installing R itself is not the expensive part; restoring and compiling the package dependency tree is.

### Package metadata strategies

There are four strategies for giving `raven check` package-export coverage in CI. Each trades off differently on R requirements, network use, coverage breadth, and version fidelity:

1. **R + packages installed** — install R and the project's packages in CI (e.g. `renv::restore()`). Exact, full coverage; version-exact; no dependency on external databases.
2. **`raven packages update`** before `raven check` — downloads broad CRAN/Bioconductor metadata from the `names-db` GitHub Release. No R needed; broad coverage; but depends on a maintainer-owned Release and tracks latest (not pinned).
3. **`raven packages fetch [--missing-only]`** before `raven check` — fetches only the project's used packages from r-universe. No R needed; no dependency on the `names-db` Release; latest exports (installed rows are version-exact under `--missing-only`). If R is unavailable in CI, `--missing-only` is a no-op and this equals plain `raven packages fetch`.
4. **`raven packages freeze`** locally + commit `.raven/packages.json` — run on a machine with R and packages installed, commit the result. No R or network needed in CI; version-exact; project-pinned.

| Strategy | Needs R in CI | Network in CI | Coverage | Version fidelity | Committed | Depends on `names-db` Release |
|---|---|---|---|---|---|---|
| 1. R + packages installed | yes | (install) | exact, full | version-exact | no | no |
| 2. `packages update` | no | yes (1 file) | whole ecosystem | latest snapshot | no | **yes** |
| 3. `packages fetch [--missing-only]` | no | yes (per-pkg) | project's used set | latest (installed rows exact under `--missing-only`) | no (ephemeral) | no |
| 4. `freeze` + commit | no (in CI) | no | project's used set | version-exact | yes | no |

Strategies 3 and 4 can be combined: commit a `freeze` file for version-exact coverage of installed packages, then run `fetch` in CI to top up whatever `freeze` missed — `fetch`'s additive merge preserves every `freeze` row untouched.

See [`raven packages fetch`](#raven-packages-fetch), [`raven packages freeze`](#raven-packages-freeze), and [Package database](package-database.md) for details.

### GitHub Actions example

You can copy [`docs/examples/ci/github-actions-raven.yml`](examples/ci/github-actions-raven.yml) to `.github/workflows/raven.yml`:

```yaml
name: Raven

# Runs on pull requests and on pushes to the default branch (main).
# Scoping push to main avoids a duplicate run when you push to a branch
# that already has an open pull request (pull_request already covers that).
"on":
  push:
    branches: [main]
  pull_request:

jobs:
  raven:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jbearak/setup-raven@v1
        with:
          version: latest
      - run: raven packages update
      - run: raven check
```

The [`jbearak/setup-raven`](https://github.com/jbearak/setup-raven) action installs Raven from the prebuilt GitHub Release binary for the runner platform, verifies the published SHA-256 checksum, and adds `raven` to `PATH`. It installs only: every `raven` command — `packages update`/`fetch`, `check`, `lint` — is an explicit `run:` step your workflow controls. Normal CI users need neither Rust nor Cargo. It supports Linux, macOS, and Windows runners on x64 and arm64. Set `version` to a specific release tag for fully reproducible CLI versions; the example tracks the latest release.

`cargo install --git https://github.com/jbearak/raven raven` is a source build, not a binary install — keep it for Raven development or custom local builds. GitHub Actions CI should prefer `jbearak/setup-raven@v1`; Bitbucket Pipelines and other Ubuntu CI should prefer the signed apt repository shown below.

For reproducible CI, commit `.raven/packages.json` generated by `raven packages freeze`. `raven packages update` restores broad CRAN/Bioconductor coverage, but it follows the moving `names-db` Release and is not version-pinned by the project.

To get installed/local package awareness and exact local package metadata in CI, install R (e.g. `r-lib/actions/setup-r`) before running `raven check`. Base R-platform symbols are embedded in the binary even without R. Broad CRAN/Bioconductor coverage requires Raven's `names.db` database, downloaded with `raven packages update`. Run that update step after installing Raven (with `setup-raven` on GitHub Actions, or with apt on Bitbucket/Ubuntu CI) so normal `raven check`, LSP startup, completion, hover, and package lookup stay network-free.

### Bitbucket Pipelines example

Use Raven's signed apt repository from a normal Ubuntu build image. You can copy [`docs/examples/ci/bitbucket-pipelines.yml`](examples/ci/bitbucket-pipelines.yml) to `bitbucket-pipelines.yml`:

```yaml
# Runs on pull requests and on pushes to the default branch (main).
# repository-push is scoped to main so pushing to a branch with an open
# pull request runs only once (via pullrequest-push), not twice.
image: ubuntu:24.04

pipelines:
  custom:
    Raven:
      - step:
          name: Raven
          script:
            - apt-get update
            - apt-get install -y ca-certificates curl
            - install -d -m 0755 /etc/apt/keyrings
            - curl -fsSL https://jbearak.github.io/apt-raven/raven-archive-keyring.gpg -o /tmp/raven-archive-keyring.gpg
            - echo "aaaee9d0c6d944091d1a78d8aeb4f93f59dc713ee1f218052add12b0d7c743cd  /tmp/raven-archive-keyring.gpg" | sha256sum -c -
            - install -m 0644 /tmp/raven-archive-keyring.gpg /etc/apt/keyrings/raven-archive-keyring.gpg
            - echo "deb [signed-by=/etc/apt/keyrings/raven-archive-keyring.gpg] https://jbearak.github.io/apt-raven stable main" > /etc/apt/sources.list.d/raven.list
            - apt-get update
            - apt-get install -y raven
            - raven packages update
            - raven check

triggers:
  repository-push:
    - condition: BITBUCKET_BRANCH == "main"
      pipelines:
        - Raven
  pullrequest-push:
    - condition: glob(BITBUCKET_BRANCH, "**")
      pipelines:
        - Raven
```

The apt repository is static HTTPS hosting for Debian repository metadata and `.deb` files; it is not a Docker image and does not run a remote installer script. The SHA-256 check pins the bootstrap keyring before apt trusts it, and the `signed-by=` keyring then makes apt verify Raven's repository metadata before it installs the package. Pin a specific Raven package version for reproducible CI:

```yaml
            - apt-cache madison raven
            - apt-get install -y "raven=${RAVEN_DEB_VERSION}"
```

Set `RAVEN_DEB_VERSION` in your CI variables to one of the versions listed by `apt-cache madison raven`, or use `latest`-style behavior by omitting the version pin. As with GitHub Actions, `raven packages update` restores broad CRAN/Bioconductor coverage but follows Raven's moving `names-db` Release; commit `.raven/packages.json` from `raven packages freeze` when package metadata should be project-pinned.

### Scope

`.R` / `.r` files and R Markdown / Quarto files (`.Rmd` / `.Rmarkdown` / `.qmd`) are all reported. When a workspace walk or a directory argument includes `.Rmd` / `.Rmarkdown` / `.qmd` files they are analyzed the same way the editor analyzes them: every R chunk body is treated as R code at its document coordinates, while prose, YAML front matter, and non-R chunks (Python, Bash, …) are ignored. Diagnostics are reported at the coordinates of the chunk body line where they occur, so the line and column in `text` / `json` / `sarif` output map directly to the document. Cross-file resolution from chunks works: a `source()` inside a chunk creates a real dependency edge, and missing-file and cross-file scope diagnostics are reported the same as from a `.R` file.

## `raven lint`

Run the native style linter against one or more paths and exit with a code suitable for CI gating.

```text
raven lint [OPTIONS] [PATHS...]
```

### Options

- `--config PATH` — explicit path to a `raven.toml` or `.lintr` (default: walk upward from CWD, discovering a `raven.toml` or non-home `.lintr`; literal `~/.lintr` is not auto-discovered, but can be used with `--config ~/.lintr`).
- `--no-config` — ignore `raven.toml` and `.lintr`; use Raven's built-in defaults.
- `--format text|json|sarif` — default `text`.
- `--max-severity off|hint|info|warning|error` — highest severity that does **not** fail the build (default `info`). Invoking `raven lint` explicitly enables the native linter, including when no config is discovered, under `--no-config`, or when the base `linting.enabled` setting is off. All other configuration still applies: per-file overrides and exclusions may disable linting, individual rule severities may be `off`, and custom rule values are retained. Native style lints default to `information`, so they pass at the default threshold but **do** gate at `--max-severity hint` or `off` — pin a lower threshold than `info` only if you intend style findings to fail the build.
- `--quiet` — suppress the trailing summary line.
- `--color auto|always|never` — when to colorize `text` output (default `auto`). See [Color output](#color-output).
- `--no-color` — alias for `--color never`.

### File encoding

`raven lint` reads each file through the same BOM-aware decoder as `raven check` and the editor: a UTF-8 byte-order mark is stripped and BOM-marked UTF-16 (LE/BE) is decoded, while legacy single-byte encodings (Latin-1 / Windows-1252) are never guessed — see [File encoding](#file-encoding) under `raven check` for the rationale. A file that isn't valid UTF-8 is reported as an **error diagnostic** (`File is not valid UTF-8: first invalid byte 0x… at offset …`) that fails the build (exit `1`) like any other error finding — it is **not** an operator error, so it no longer exits `2` with a cryptic decode message. Re-save the file as UTF-8 to fix it.

### Exit codes

- `0` — no diagnostic exceeded `--max-severity`.
- `1` — at least one diagnostic exceeded `--max-severity`. An unknown flag is a usage error and exits `1`. A file that isn't valid UTF-8 is an error diagnostic, so it also exits `1`.
- `2` — operator error detected while running (config parse failure, unreadable or missing path). A *readable* but mis-encoded file is a finding (exit `1`), not an operator error.

### GitHub Actions example

```yaml
- uses: actions/checkout@v4
- uses: jbearak/setup-raven@v1
  with:
    version: latest
- run: raven lint .
```

### Scope

`raven lint` runs the native style linter only. Cross-file, undefined-variable, and package diagnostics need a workspace scan; use [`raven check`](#raven-check) for those.

`.R` / `.r` files and R Markdown / Quarto files (`.Rmd` / `.Rmarkdown` / `.qmd`) are linted. Inside `.Rmd` / `.Rmarkdown` / `.qmd` files, lint rules apply to R chunk bodies; prose, YAML front matter, and non-R chunks are ignored. `# nolint` / `# nolint start` / `# nolint end` and `# raven: ignore` markers inside chunk bodies work as in plain R files. Other file types are ignored silently. Passing a directory walks it recursively for `.R` / `.r`, `.Rmd`, `.Rmarkdown`, and `.qmd` files.

## `raven packages`

A command group for the databases Raven uses to resolve package symbols without installing packages. See [Package database](package-database.md) for the full three-tier model. Both `freeze` and `fetch` are **producers** of the committed `.raven/packages.json` — `freeze` captures exports from a local R install (version-exact, committed), while `fetch` sources them from CRAN/Bioconductor r-universe (latest, ephemeral). `update` downloads broad CRAN/Bioconductor metadata (it isn't bundled with the binary). The maintainer-only commands that build the shipped databases (`build-shipped-db`, `validate-shipped-db`, `build-embedded-base`) are documented in the [appendix](#appendix-maintainer-and-advanced-topics).

**Top-level aliases.** `raven freeze [ARGS]` is exactly equivalent to `raven packages freeze [ARGS]`, and `raven fetch [ARGS]` to `raven packages fetch [ARGS]`. They are thin routing aliases — same parsing, same handler, same help. Only `freeze` and `fetch` are aliased; `update` and `build-shipped-db` stay nested.

### `raven packages fetch`

Produce a `.raven/packages.json` from CRAN/Bioconductor r-universe — the same artifact `freeze` produces, but sourced from community infrastructure instead of a local R install. It needs no R, no installed packages, and no dependency on the `names-db` GitHub Release. The file is an **ephemeral CI artifact** meant to be regenerated each run; gitignore it rather than committing it (contrast with `freeze`'s committed, version-pinned file).

```text
raven packages fetch [OPTIONS]
```

**Two modes:**

- **Plain `raven packages fetch`** — computes the used set without R (a tree-sitter scan of your sources, plus `DESCRIPTION` `Depends`/`Imports`, `renv.lock` names, and transitive `Depends` from r-universe), skips base/recommended packages (known offline via the embedded base set), and fetches the rest from r-universe. No R required.
- **`raven packages fetch --missing-only`** — a pure optimization. When R is present with a populated library, it subtracts already-installed packages from the fetch set (they will resolve from your local R library in the same CI run). When R is absent or `.libPaths()` is empty, nothing is subtracted and `--missing-only` degrades to a full fetch. It is **never an error** to pass `--missing-only` without R.

#### Options

- `--missing-only` — subtract already-installed packages from the fetch set. No-op without R (degrades to a full fetch).
- `--fail-on-missing` — exit non-zero if any used package resolved nowhere (after writing the file). By default, unresolvable packages produce warnings and a success exit.
- `--output PATH` — where to write/merge (default: `.raven/packages.json` at the workspace root).
- `--workspace DIR` — workspace root to scan for usage and config (default: current directory).
- `--base-urls URL[,URL]` — override the ordered r-universe host list (for testing); each URL must include the scheme. Default: `https://cran.r-universe.dev,https://bioc.r-universe.dev`.
- `--help` — usage.

#### Additive merge — existing records always win

`fetch` reads any existing `.raven/packages.json` at the target path and **preserves every record in it untouched**, adding records only for used packages not already present. It does not even refetch a package the existing file already covers. Run after `freeze`, it tops up coverage for whatever `freeze` missed (e.g. uninstalled packages) without disturbing a single `freeze` row. When the merge produces content identical to the existing file, `fetch` leaves the file untouched and prints "no changes."

A corrupt or unsupported-schema existing file is surfaced as an error and the command refuses — it won't silently discard a file you may need. Delete it or point `--output` elsewhere.

#### Output and warnings

- An **inform** line names the packages about to be downloaded.
- Per-package **warnings** for packages that resolve nowhere on r-universe (GitHub-only, internal, not-yet-indexed, or typos like `library(dpylr)`).
- A **renv.lock version-skew warning** when the fetched record's version differs from the version `renv.lock` pins. r-universe serves latest only — it does not archive old versions — so `fetch` cannot pull the exact pinned version. The warning names both versions and points at `freeze` / `--missing-only` for a version-exact capture. Export names are usually stable across versions, so this is a soft heads-up, never an error, and never gated by `--fail-on-missing`.

#### `--fail-on-missing`

By default, packages that resolve nowhere produce warnings and a success exit. With `--fail-on-missing`, a non-empty resolved-nowhere set makes the command exit non-zero **after** writing the file (so partial results are still available). Suppression of expected-missing private/internal packages is a planned fast-follow; v1 warns them every run.

#### Gitignore guidance

The fetched file is an ephemeral CI artifact — regenerated each run from the latest r-universe exports. **Gitignore it** (add `.raven/packages.json` to `.gitignore`) rather than committing it. A user *may* commit it, but that is not the design target: for a committed, version-pinned artifact, use `freeze` instead.

#### Network and atomicity

Fetches via curl with bounded concurrency, trying CRAN then Bioconductor per package. Writes atomically (temp-then-rename) so a mid-fetch failure cannot feed a half-written file to `raven check`. If every fetch fails at the transport level (network down / curl missing), the command reports the failure and writes nothing — any existing file is left intact. A partial failure writes what it got and warns about the rest.

#### Scope limits

Two honest limits:

1. `fetch` does **not** replace Raven's broad, whole-ecosystem `names.db` metadata — it covers only packages the project references (the "used set").
2. `fetch` is **not** fully self-contained — base/recommended packages are not on r-universe and still come from local R or the embedded fallback at analysis time.

### `raven packages freeze`

Generate a committed, repo-specific `.raven/packages.json` — a snapshot of your installed packages' export names, `Depends`, and datasets. This is Raven's reproducible CI path: run it on a machine that has R and the project's packages installed, then commit the result so CI uses project-pinned package metadata. Raven's bundled or updated CRAN/Bioconductor metadata provides broad ecosystem coverage when available, but this committed snapshot **improves accuracy** for packages the broad metadata doesn't cover (GitHub-only or internal packages) and for symbols newer than the metadata snapshot. The file is generated, never hand-edited.

```text
raven packages freeze [OPTIONS]
```

#### Options

- `--used` (default) — capture only the packages the repo uses. The set is deliberately **maximally inclusive** — it combines packages referenced via `library`/`require`/`loadNamespace`/`requireNamespace`, the left-hand side of `::`/`:::`, everything in `renv.lock`, the repo's own `DESCRIPTION` `Depends`/`Imports`, and their transitive `Depends`. (`LinkingTo` is excluded — it is C-level and has no R exports.) Over-inclusion is free: the capture skips anything not actually installed.
- `--installed` / `--all` — capture every package across the renv and system libraries, not just the used set.
- `--output PATH` — where to write the file (default: `.raven/packages.json` at the workspace root).
- `--workspace DIR` — workspace root to scan for usage and config (default: current directory).

#### Base packages

`freeze` skips only the seven packages R attaches by default — the ones Raven treats as always in scope with no `library()` call. It may still write records for other base packages such as `grid`, `tools`, and `compiler` when your code uses them or when you choose `--installed` / `--all`. That is intentional: `freeze` captures your local R install, which may differ from the reference R used to build Raven's embedded fallback, and the generated file should match packages you explicitly call in scripts.

#### Library order and renv

Generation resolves each package from a **renv-first** library order: the renv project library first, system-wide libraries only for packages renv doesn't cover (renv wins, system fills the gaps). A `renv.lock` acts purely as a **set selector** — it decides *which* packages to include (a locked package is included even if no script ever calls it), **not** which version to read; the exports always come from the package actually installed locally. A locked package that isn't installed simply can't be captured and falls through to Raven's bundled CRAN/Bioconductor metadata in CI. Best coverage therefore comes from running `freeze` after `renv::restore()`, but nothing breaks otherwise.

#### No-op when content is unchanged

If a `.raven/packages.json` already exists, `freeze` compares **package content only** (ignoring provenance such as the generation timestamp). When the content is identical it leaves the file untouched and prints "no changes" — so a regeneration that found nothing new produces a zero-line diff, and the provenance timestamp moves only when the captured exports actually changed.

### `raven packages update`

Downloads the `names.db` database Raven uses to recognize package symbols when the packages are not installed, from the `names-db` GitHub Release into Raven's user data directory. Base R-package coverage is embedded in the binary and needs no download. `names.db` is **not** bundled with the `raven` binary — use this whenever you want broad CRAN/Bioconductor coverage without a local R install. (The VS Code extension doesn't need it: VS Code users resolve their installed packages through R.) This command is the explicit network boundary for package metadata: `raven check`, LSP startup, completion, hover, and normal package lookup do not fetch it.

In GitHub Actions, install Raven with `jbearak/setup-raven@v1` and then run this as an explicit step before `raven check`. In Bitbucket Pipelines or other Ubuntu CI, install Raven from the signed apt repository first, then run the same update/check steps:

```sh
apt-get install -y raven
raven packages update
raven check
```

Source builds with `cargo install --git https://github.com/jbearak/raven raven` are for Raven development or custom images, not the recommended hosted-CI path.

For reproducible CI, commit `.raven/packages.json` generated by `raven packages freeze`. `raven packages update` restores broad CRAN/Bioconductor coverage, but it follows the moving `names-db` Release and is not version-pinned by the project.

To pin a reproducible snapshot, pass an immutable dated release instead of tracking the moving one:

```sh
raven packages update 2026-06-02
```

Each build also publishes an immutable `names-db-YYYY-MM-DD` Release alongside the rolling `names-db` one, and these dated snapshots are retained indefinitely. With no date argument, `update` pulls the latest (`names-db`).

The maintainer-only `raven packages` commands (`build-shipped-db`, `validate-shipped-db`, `build-embedded-base`) are documented in the [appendix](#appendix-maintainer-and-advanced-topics).

## Output formats

Both `check` and `lint` share the same renderers:

- `text` — `path:line:col level: message [rule]`, one per line.
- `json` — array of `{ path, diagnostic }` objects (`diagnostic` is a verbatim LSP `Diagnostic`).
- `sarif` — SARIF 2.1.0 envelope. Tool name `raven`; `ruleId` from `Diagnostic.code`.

### Output streams

Diagnostics go to **stdout** for both commands. Beyond the diagnostics themselves, `raven check` may print a handful of **context notes** — short prose that explains *why* a result might be incomplete or how to act on it (`raven lint` prints none of these). They fall into two groups by *when* they appear:

- **Startup notes** — printed once, *before* any diagnostics, to **stderr**. They report a degraded environment that affects the whole run.
- **Footer notes** — printed once, *after* all diagnostics, as a footer. They annotate the findings above them. For `text` they go to **stdout** (the diagnostics' own stream); for `json` / `sarif` they go to **stderr** so they can't corrupt the machine document on stdout. The footer is set off from the summary line by a blank line and carries no header or per-line prefix (it is `raven check`'s own output, so labeling it as such mid-stream would be redundant); when more than one note fires, they are separated by a blank line.

Footer notes share the diagnostics' stream for `text` deliberately: stdout and stderr are independent streams that a merged consumer (a terminal, `2>&1`, or a CI log viewer such as GitHub Actions, which timestamps each line as it reads it) can reorder, which would interleave a multi-line note with the findings it describes. One stream keeps them grouped and in order — so a note never refers to another by position across streams.

#### What `raven check` can print, and when

Each note below is printed only when its condition holds; a clean run on a fully-resolved workspace prints just the diagnostics and the summary line.

**Startup notes (stderr, before diagnostics)** — one fires when R-backed package resolution is degraded **and** no offline package data is available to cover for it, so undefined-variable findings for package symbols may be unreliable. The text names the cause and the consequence.

**The note is keyed on actual coverage, not merely on R's absence.** It is suppressed when offline package data loaded — an updated `names.db` (Tier 3 of the [package-resolution fallback](package-database.md)) or a frozen `.raven/packages.json` (Tier 2) — because that data resolves package symbols without a live R library, so the warning would be a false alarm (and would tell a CI that *already ran* `raven packages update` to run it again). It fires only when coverage falls back to base R alone.

All three variants share the same tail — ``package symbol resolution is limited to base R (covered by the embedded database). Run `raven packages update` for broad CRAN/Bioconductor coverage`` — because the limitation and its remedy are identical regardless of *why* the live R library was unavailable.

- R not found on `PATH` — ``R not found on PATH; package symbol resolution is limited to base R (covered by the embedded database). Run `raven packages update` for broad CRAN/Bioconductor coverage``.
- R found but its package library failed to initialize — ``R found but its package library failed to initialize (…); package symbol resolution is limited to base R (…). Run `raven packages update` …``.
- R found but no library paths were discovered — ``R found but no library paths were discovered; package symbol resolution is limited to base R (…). Run `raven packages update` …``.

**Footer notes (stdout for `text`, stderr for `json`/`sarif`, after diagnostics)**, in the order printed:

1. **Package-database load note** — fires when a package symbol database is *present but unusable* (e.g. a malformed or unreadable committed `.raven/packages.json`, or a corrupt `names.db`). It names the specific load failure. *Why:* the database silently wasn't searched, so some symbols may be unresolved — distinguishing this from a genuine typo.
2. **Missing-export-metadata warning** — fires when attached packages' exported symbols couldn't be loaded (`couldn't load exported symbols for <packages>. Some "Undefined variable" warnings above may be inaccurate …`), followed by a fix tailored to *why* the metadata was missing (install the package, run `raven packages freeze`, or run `raven packages update` in CI). *Why:* without a package's exports, calls into it can produce false undefined-variable findings.
3. **Cross-file traversal-budget note** — fires when a bounded cross-file traversal was truncated, either by the visited-node budget (`maxTransitiveDependentsVisited`) or the chain-depth limit (`maxChainDepth`). It names the budget hit and how to raise it in `raven.toml`. *Why:* a truncated traversal stops following `source()` edges, so symbols defined across a dropped edge may appear as false-positive undefined-variable warnings — the note lets CI tell a budget-induced drop apart from a real undefined variable.
4. **NSE-discoverability footer** — see below.

#### NSE-discoverability footer

When some undefined-variable findings sit inside call arguments that Raven cannot see into (a package function that *might* capture the argument via [non-standard evaluation](non-standard-evaluation.md)), the **`text`** report prints one footer after the findings — and **only** there. The footer is framed carefully so it never reads as Raven asserting the call *is* NSE: it leads with the universal false-positive escape hatches every linter has (`# raven: ignore`, `# nolint`, `# raven: expect`) and presents NSE as the *one R-specific* additional cause. It then lists the distinct, copy-pasteable per-function directives (one per function, however many findings it caused) and links [the directives reference](directives.md) and [handling false positives](diagnostics.md). The snippet is enough to apply without understanding the syntax; the links explain it — mirroring how ShellCheck and Clippy attach a docs URL rather than explaining suppression inline.

It reads (with concrete callees filled in), as a footer after the findings:

```
3 undefined-variable findings above sit inside calls to package functions
whose source raven can't see. If one is a false positive, you can suppress it as with any
linter (`# raven: ignore`, `# nolint`, or `# raven: expect`).
R has one extra cause: a function that captures an argument via non-standard
evaluation (NSE) — as `dplyr::filter(df, col > 0)` treats `col` as a column, not a
variable — makes a valid name look undefined. Raven already recognizes NSE in many
common packages (the tidyverse and more) but not all, so these findings come from
functions outside that built-in coverage. If that is the case here, declare the
function's NSE contract instead of suppressing:

  # raven: nse somepkg::my_filter(x)
  # raven: func somepkg::other(<formals>)
  # raven: nse somepkg::other(<nse-formals>)

The two-line `# raven: func …` / `# raven: nse …` pair is for an argument passed positionally:
raven needs the function's parameter list to know which formal the argument is, so fill
`<formals>` with the function's signature and `<nse-formals>` with the captured ones. Keep them
on separate lines — each `# raven:` directive must be the only one on its line. When
the argument is passed by name, naming that formal (`# raven: nse fn(x)`) is enough.

See https://github.com/jbearak/raven/blob/main/docs/directives.md for these directives and
https://github.com/jbearak/raven/blob/main/docs/diagnostics.md for handling false positives.
```

(Named-formal suggestions are listed first; the wordier positional form, with the explanation above, last.)

The suggestion is **not** repeated on each finding, and it does **not** appear in the diagnostic message at all: the editor and the `json` / `sarif` formats carry no NSE prose (their messages stay the bare "`x` is not defined"). In the editor there is no per-finding hint or code action — the editor diagnostic stays the bare "`x` is not defined", and the NSE suggestion is `raven check` text-footer only.

## Color output

Both `check` and `lint` colorize the **severity word** (`error`, `warning`, `info`, `hint`) in `text` output — the rest of the line stays uncolored so it remains easy to grep. The `json` and `sarif` formats are machine-readable and are **never** colorized regardless of the flag or environment.

`--color auto|always|never` controls it (default `auto`), and `--no-color` is a familiar alias for `--color never`. Resolution precedence, highest first:

1. An explicit `--color always` / `--color never` (or `--no-color` ⇒ `never`) always wins, overriding the environment.
2. Otherwise (`auto`):
   - [`NO_COLOR`](https://no-color.org/) set and non-empty ⇒ **off**.
   - else [`FORCE_COLOR`](https://force-color.org/) set and non-empty ⇒ **on**.
   - else **on** when stdout is a terminal, **off** when piped or redirected.

So `--color always` forces color even through a pipe (e.g. into `less -R` or a CI log viewer), `--color never` / `--no-color` / `NO_COLOR` suppress it, and the default tracks whether you're looking at a terminal. Conflicting explicit flags are last-one-wins (`--no-color --color always` ⇒ color on).

## Appendix: maintainer and advanced topics

These topics are for maintainers and advanced / organizational setups; most users never need them.

### Self-hosting `names.db` with `--base-url`

`raven packages update --base-url URL` overrides where the database is fetched from. The command downloads `{URL}/names.db` (any trailing slash on `URL` is trimmed), so an organization can host its own copy on an internal mirror:

```sh
raven packages update --base-url https://mirror.example.internal/raven
# fetches https://mirror.example.internal/raven/names.db
```

Notes:

- The URL must be `http://` or `https://` (other schemes are refused). Redirects are followed. The download is capped at 200 MB — purely defensive: that ceiling is an order of magnitude beyond any real `names.db`, so it should never be reached in normal use. It only guards against a misconfigured endpoint or a wrong URL serving something far larger than expected.
- The downloaded file is validated structurally before it replaces the existing database file — `update` opens it as a Raven DB (container header, format version, payload checksum, index bounds, decodable records). This confirms the file is a well-formed `names.db`, **not** who produced it; there is no signature or provenance check, and the published `checksums.sha256` is not fetched. Trust rests on the transport (prefer HTTPS) plus that structural validation.
- `--base-url` is a per-invocation flag, not a persisted setting (`RAVEN_NAMES_DB` is unrelated — it overrides the database *location*, not the download source). Wrap it in a script or CI step for repeated use.
- `--base-url` is a full override and is therefore mutually exclusive with the `YYYY-MM-DD` dated-release argument; a self-hosted mirror defines its own URL layout.

### `raven packages build-shipped-db`

**Maintainer / CI-only — most users never run this.** Builds Raven's Tier 3 `names.db` database. All base-priority packages are excluded from `names.db` (they are embedded in the binary). The shipped binary is **network-free**: this command transforms r-universe JSON that the build workflow has already downloaded with `curl`, merging it **append-only** over an authoritative reference-R capture of the build machine's installed library. The reference capture **auto-discovers every R `.libPaths()` entry**. The result is published to the `names-db` GitHub Release, where `raven packages update` downloads it on demand — it is **not** bundled with the binary, and the VSIX omits it too (VS Code users resolve their locally installed packages via Tier 1). See [Package database](package-database.md#tier-3--namesdb-database) and [development.md](development.md#tier-3-build-pipeline) for the build pipeline.

### `raven packages validate-shipped-db`

**Maintainer / CI-only — most users never run this.** Opens a `names.db` database with the current Raven binary, verifies the container header, format version, payload checksum, index bounds, and decodes every package record. The command fails if the file is corrupt, uses a newer unsupported format, or if the fully decoded record count does not match the database provenance.

```text
raven packages validate-shipped-db names.db
```

Raven's release workflow runs this against the current `names-db` Release asset before building the release binaries — a **compatibility gate** confirming the version of Raven being shipped can open and validate the database users will download. It is not a bundling step: `names.db` is not shipped with the binary.

### `raven packages build-embedded-base`

**Maintainer-only — most users never run this.** Regenerates the embedded base export/dataset table (`crates/raven/src/package_db/embedded_base_generated.rs`) from a reference R installation — all 14 base-priority packages. The generated file is compiled into the binary so base symbols are available without any database file or R installation.

```text
raven packages build-embedded-base --reference-lib DIR [--output PATH]
```

- `--reference-lib DIR` — path to the R library containing the base packages (e.g. the output of `Rscript -e 'cat(.Library)'`).
- `--output PATH` — where to write the generated file (default: `crates/raven/src/package_db/embedded_base_generated.rs`).

After running, verify with `cargo test -p raven embedded_base` and commit the result.
