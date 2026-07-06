# Automated checks in CI

CI means **Continuous Integration**: a service such as GitHub Actions or Bitbucket Pipelines runs commands automatically when you push code or open a pull request. In practice, it gives every change a clean, repeatable check before it is merged. For a research or analysis repository, that means a typo, a missing `source()` target, or an undefined object can be caught during review instead of later, when someone reruns the analysis.

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

on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review]
  push:

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

## What fails the build

By default, `raven check` exits with code `1` when it finds a `warning` or `error` diagnostic, and `0` otherwise. Change that threshold with `--max-severity`:

```bash
raven check --max-severity warning
```

That command allows warnings and fails only on errors. See [Exit codes](cli.md#exit-codes) and [Diagnostics](diagnostics.md) for the full diagnostic set.

Use a committed [`raven.toml`](configuration.md) to keep local editor diagnostics and CI behavior aligned. Common CI-specific configuration includes `[workspace].exclude` for generated outputs and `diagnostics.reportUnusedSuppressions = true` when you want Raven to flag stale `# raven: ignore` comments.
