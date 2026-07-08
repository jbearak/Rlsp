# Automated checks in CI

CI means **Continuous Integration**: a service such as GitHub Actions or Bitbucket Pipelines runs commands automatically every time you push code or open a pull request. You describe the commands once, in a small configuration file committed to the repository, and the service runs them on a fresh machine for every change — no one has to remember to run the checks, and the result is visible to everyone reviewing the change.

If you have not worked with CI before, it may help to know that this is standard practice in most software ecosystems: Python projects run `pytest` and `ruff` on every pull request, TypeScript projects run `eslint` and the compiler, Rust projects run `cargo clippy` and their test suite. The tools differ, but the pattern is the same — an automated gate that catches problems before a change is merged, instead of after it has broken something. Raven brings that same pattern to R analysis code. For a research repository, it means a typo, a missing `source()` target, or an undefined object is caught during review instead of later, when someone reruns the analysis.

The mechanics, concretely:

1. You commit a configuration file to the repository — `.github/workflows/raven.yml` for GitHub Actions, or `bitbucket-pipelines.yml` at the repository root for Bitbucket. The copy-paste examples below are complete files; you do not need to write YAML from scratch.
2. When you push a commit or open a pull request, the service starts a fresh Linux machine, checks out your code, and runs the listed commands.
3. The result appears on the pull request as a green check mark or a red ✗. Clicking through shows the log — the same output you would see running `raven check` in a terminal, including file names and line numbers for each finding.

Raven's CI command is [`raven check`](cli.md#raven-check). It runs the same static diagnostics the editor publishes, but in a headless batch suitable for CI logs and pass/fail gating. It reads `.R`, `.Rmd`, and `.qmd` files, follows `source()` chains, and reports syntax errors, missing source paths, undefined variables, package-scope issues, and enabled style lints. It does **not** execute your scripts.

## Recommended setup

For most analysis repositories, start with this pattern:

1. Install the Raven CLI in the CI job: use `jbearak/setup-raven` on GitHub Actions, or the signed apt repository on Bitbucket Pipelines.
2. Run `raven packages update` so Raven can recognize CRAN and Bioconductor package exports without installing the packages.
3. Run `raven check`.

That setup needs no R installation and does not run your analysis. Raven only needs package **export names** so it can tell package functions from undefined variables. Base R symbols are embedded in the binary; broad CRAN/Bioconductor coverage comes from `raven packages update`.

Use a committed package database instead when you need reproducible, project-pinned package metadata:

```bash
raven packages freeze
git add .raven/packages.json
```

Generate that file locally on a machine with R and the project's packages installed, commit it, and CI can run `raven check` without `raven packages update`. See [Package database](package-database.md) and [CI package metadata strategies](cli.md#package-metadata-strategies) for the full trade-off table.

If your pipeline also installs packages and runs the R scripts, add `--report-uninstalled` so `library(pkg)` calls fail when the package is absent from the CI library:

```bash
raven check --report-uninstalled
```

For a static gate that does not execute the scripts, keep the default. It suppresses missing-package warnings because CI often checks code without restoring the whole R library.

## GitHub Actions

Use [`jbearak/setup-raven`](https://github.com/jbearak/setup-raven) to install the prebuilt Raven CLI. You can copy [`docs/examples/ci/github-actions-raven.yml`](examples/ci/github-actions-raven.yml) to `.github/workflows/raven.yml`:

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

Pin `version` to a release tag when you want a fully reproducible CLI version.

## Bitbucket Pipelines

Bitbucket Pipelines runs the commands in `bitbucket-pipelines.yml`. Raven publishes a signed apt repository, so a normal Ubuntu build image can install the CLI directly. You can copy [`docs/examples/ci/bitbucket-pipelines.yml`](examples/ci/bitbucket-pipelines.yml) to `bitbucket-pipelines.yml`:

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

Pin a specific Raven package version for reproducible Bitbucket runs:

```yaml
            - apt-cache madison raven
            - apt-get install -y "raven=${RAVEN_DEB_VERSION}"
```

Set `RAVEN_DEB_VERSION` in your pipeline variables to one of the versions listed by `apt-cache madison raven`, or omit the version pin to track the latest package in the apt repository.

If VS Code's YAML extension reports an unresolved Bitbucket schema reference such as `pipelines_configuration`, the pipeline file can still be valid. That is an editor schema issue, not a Raven or Bitbucket runtime error.

If the repository stores large data files in [Git LFS](https://git-lfs.com), skip downloading them — `raven check` reads only code, and analysis datasets can dwarf it:

```yaml
clone:
  lfs: false
```

Add that at the top level of `bitbucket-pipelines.yml`. (On GitHub Actions no change is needed: `actions/checkout` skips LFS content by default.)

## Running Raven and Sight together

Some social-science repositories mix R and Stata. [Sight](https://github.com/jbearak/sight) is Raven's sibling project — the same kind of static checker, for Stata `.do` files — and its CI command, `sight check`, works just like `raven check`. If your repository contains both languages, run both checkers.

The examples below run them **sequentially in a single job**, and that ordering is deliberate: each additional job spins up its own fresh machine and checks out the repository again, so two separate jobs cost roughly twice the startup time and compute minutes to run two commands that each take seconds. One machine, one checkout, two checks is the efficient shape — both checkers take seconds, so splitting them across runners buys nothing. (If you prefer a separate green/red mark per checker on the pull request, both providers also support splitting them into parallel jobs; that alternative is described after each example.)

### GitHub Actions

You can copy [`docs/examples/ci/github-actions-raven-and-sight.yml`](examples/ci/github-actions-raven-and-sight.yml) to `.github/workflows/checks.yml`:

```yaml
name: Checks

# Runs on pull requests and on pushes to the default branch (main).
# Scoping push to main avoids a duplicate run when you push to a branch
# that already has an open pull request (pull_request already covers that).
"on":
  push:
    branches: [main]
  pull_request:

jobs:
  checks:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: jbearak/setup-raven@v1
        with:
          version: latest
      - uses: jbearak/setup-sight@v1
        with:
          version: latest
      - run: raven packages update
      - run: raven check
      # if: always() means Sight still runs (and reports its findings)
      # even when raven check has already failed the job.
      - run: sight check
        if: always()
```

Both checkers install onto the same runner and share one checkout. The `if: always()` on the Sight step matters: without it, a Raven finding would skip the Sight step entirely, so you would fix the R issues, push, and only then discover the Stata issues. With it, one run reports both.

To run them as **parallel jobs** instead, put two entries under `jobs:` — one with the Raven steps, one with the Sight steps — each starting with its own `actions/checkout@v4`. Each job gets its own runner and its own status on the pull request.

### Bitbucket Pipelines

Sight is distributed on npm, so the combined pipeline uses the `node:24` image (Debian-based — Raven's signed apt repository installs on it exactly as on Ubuntu) and adds Sight with one `npm install`. You can copy [`docs/examples/ci/bitbucket-pipelines-raven-and-sight.yml`](examples/ci/bitbucket-pipelines-raven-and-sight.yml) to `bitbucket-pipelines.yml`:

```yaml
# Runs on pull requests and on pushes to the default branch (main).
# repository-push is scoped to main so pushing to a branch with an open
# pull request runs only once (via pullrequest-push), not twice.
image: node:24

pipelines:
  custom:
    Checks:
      - step:
          name: Raven and Sight
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
            - npm install -g @jbearak/sight
            - raven packages update
            # Run both checkers even if the first one finds issues, then fail
            # the step if either did.
            - raven check || touch /tmp/raven-failed
            - sight check
            - test ! -f /tmp/raven-failed

triggers:
  repository-push:
    - condition: BITBUCKET_BRANCH == "main"
      pipelines:
        - Checks
  pullrequest-push:
    - condition: glob(BITBUCKET_BRANCH, "**")
      pipelines:
        - Checks
```

Bitbucket stops a step at the first failing command, so the last three lines use a small marker-file pattern to let Sight run even when Raven has findings, and still fail the step if either checker did. Pin versions for reproducible runs the same way as in the Raven-only example: `apt-get install -y "raven=${RAVEN_DEB_VERSION}"` for Raven, and `npm install -g @jbearak/sight@X.Y.Z` for Sight.

To run them as **parallel steps** instead, wrap two `- step:` entries in a `- parallel:` block — one installing and running Raven, one installing and running Sight. Each step gets its own container and its own checkout, and each reports separately in the pipeline view.

## What fails the build

"Fails the build" does not mean Raven itself failed. It means `raven check` ran successfully and found at least one diagnostic at or above your severity threshold, and it reports that through its exit code — the standard way a checker signals GitHub Actions or Bitbucket Pipelines that there is something to look at. The three exit codes are distinct:

- **`0`** — nothing exceeded the threshold; the step passes.
- **`1`** — a diagnostic exceeded the threshold; the step fails. This is the signal you want — Raven found an issue in your code, not that Raven broke.
- **`2`** — Raven itself could not run (an unreadable path, a malformed `raven.toml`). This is a genuine operator error, separate from finding issues in your code.

`--max-severity LEVEL` sets the highest severity still allowed to pass; anything more severe fails the build. From most to least severe the scale is `error`, `warning`, `info`, `hint`. The default is `info`, so `warning` and `error` findings fail while `info` and `hint` pass.

Raven's style and idiomatic lints — line length, naming, infix spacing, and similar — are `information`-level, so they never fail CI by default. To gate on them, first enable linting (via `raven.toml` or a `.lintr` — see [Linting](linting.md)), then lower the threshold to `hint` so info-level findings fail:

```bash
raven check --max-severity hint
```

That fails the build on style findings as well as warnings and errors; `--max-severity off` is stricter still and fails on every diagnostic. See [Linting](linting.md), [Exit codes](cli.md#exit-codes), and [Diagnostics](diagnostics.md) for the full rule and diagnostic set.

A failing `raven check` fails the pipeline, but a red build does not block a merge on its own. To prevent merging when the check fails, mark it as a required status check (GitHub branch protection) or a merge check (Bitbucket).

Use a committed [`raven.toml`](configuration.md) to keep local editor diagnostics and CI behavior aligned. Common CI-specific configuration includes `[workspace].exclude` for generated outputs and `diagnostics.reportUnusedSuppressions = true` when you want Raven to flag stale `# raven: ignore` comments.
