# Development Notes

This document is for contributors/maintainers (and LLMs) working on Raven’s Rust codebase. User-facing behavior is documented in `README.md` and `docs/`.

## Where to look first

User-facing docs:
- `README.md`
- `docs/cross-file.md`
- `docs/r-package-dev.md`
- `docs/directives.md`
- `docs/diagnostics.md`
- `docs/diagnostic-corpora.md`
- `docs/indentation.md`
- `docs/r-console.md`
- `docs/plot-viewer.md`
- `docs/data-viewer.md`
- `docs/help-viewer.md`
- `docs/document-outline.md`
- `docs/coexistence.md`
- `docs/configuration.md`

Key code entry points:
- Cross-file metadata extraction: `crates/raven/src/cross_file/mod.rs` (`extract_metadata()`)
- Dependency graph: `crates/raven/src/cross_file/dependency.rs`
- Scope artifacts + interface hash: `crates/raven/src/cross_file/scope.rs`
- Path resolution: `crates/raven/src/cross_file/path_resolve.rs`
- Real-time updates / diagnostics gating: `crates/raven/src/cross_file/revalidation.rs`
- On-demand indexing (synchronous, in `did_open`): `crates/raven/src/backend.rs` (`index_file_on_demand`, `index_forward_chain`, `index_backward_chain`)
- Package loading: `crates/raven/src/package_library.rs`
- Namespace member authority: `crates/raven/src/package_library.rs` (`namespace_member_status_sync`) — the single source of truth for the `namespace-member-not-found` diagnostic. **Not** `get_exports_sync` (which powers `pkg::` completion). It is synchronous and never spawns R or mutates the cache, so a not-yet-warmed package returns `Unknown` and the diagnostic surfaces only after background warming republishes. Exports-completeness is stamped at every load path and never downgraded by `insert_package` (see `docs/package-database.md`). Because warming `pkg` can change `pkg::member` diagnostics in *other* open docs, the `did_change` background prefetch force-republishes siblings found via `open_docs_referencing_packages` (`backend.rs`, issue #503).
- Package state: `crates/raven/src/package_state/` (`PackageState`, `derive_package_state()`)
- Package namespace: `crates/raven/src/package_namespace.rs` (workspace detection, namespace model)
- Help rendering: `crates/raven/src/help/` (Rd2txt, Rd2HTML)
- Libpath watching: `crates/raven/src/libpath_watcher.rs`
- Qualified member resolution: `crates/raven/src/qualified_resolve.rs`
- Parameter resolution: `crates/raven/src/parameter_resolver.rs`
- Roxygen parsing: `crates/raven/src/roxygen.rs`
- Content provider: `crates/raven/src/content_provider.rs`

## Build from source

Prerequisites:
- Rust toolchain (MSRV: 1.96; the workspace is on the 2024 edition)

Clone and build:
```bash
git clone https://github.com/jbearak/raven.git
cd raven
cargo build --release -p raven
# Binary will be at target/release/raven
```

## Build, test, run

- Debug build: `cargo build -p raven`
- Release build: `cargo build --release -p raven`
- Run (stdio): `RAVEN_PERF=1 cargo run -p raven -- --stdio`
- Unit tests: `cargo test -p raven`
- Integration tests: `cargo test -p raven --features test-support -- --test`
- Bun tests (TypeScript): `bun test` (from repo root; `bunfig.toml` sets root to `./tests/bun`)
- VS Code extension tests: `cd editors/vscode && bun run test`
- Setup (build + install + package): `./scripts/setup.sh`

The VS Code extension tests include a real-layout webview harness for the
data-viewer toolbar's responsive chip-wrap behavior
(`editors/vscode/src/test/toolbar-wrap-layout.test.ts` +
`toolbar-wrap-harness-panel.ts`). The harness mounts the production
toolbar JSX in a webview, pins its width via `postMessage`, and posts the
measured geometry back to the host for assertions. The harness bundle
lives at `editors/vscode/dist-test/toolbar-wrap-harness/` — separate from
the shipped `dist/`, built by `bun run bundle:webview-test` (already
wired into `pretest`), excluded from the VSIX. Set
`RAVEN_SKIP_LAYOUT_TESTS=1` to skip the suite in sandboxes that can't
render a webview.

### Benchmarks

All benchmarks except `startup` require `--features test-support`. Set `RAVEN_BENCH_ALLOC=1` for allocation tracking.

- `cargo bench --bench startup`
- `cargo bench --bench lsp_operations --features test-support`
- `cargo bench --bench cross_file --features test-support`
- `cargo bench --bench libpath_capture --features test-support`
- `cargo bench --bench edit_to_publish --features test-support`
- `cargo bench --bench indentation --features test-support`

The `Performance` GitHub Actions workflow tracks `startup`, `indentation`, and
the standalone-cache subset of `cross_file` for PR comparison comments.
Criterion's `main` baseline is cached from push-to-`main` runs only; PR runs
restore that cache and save their results as a local `pr` baseline, but must
not write the `target/criterion` cache. Allowing PRs to save the cache can
create branch-scoped entries that contain only `pr`, which then shadow the
default-branch cache and make future PRs report "No `main` baseline found".
The PR comparison guard must use `critcmp --baselines` to detect saved baseline
names; `critcmp --list` formats comparison output and does not prove that a
restored `main` baseline exists.

## Profiling startup

Python scripts under `scripts/` measure LSP startup latency:
- `scripts/profile_startup.py`: spawns the LSP, opens files, measures time to first diagnostic
- `scripts/profile_simple.py`: simpler initialization timing

Typical flow:
- `cargo build --release -p raven`
- `python3 scripts/profile_startup.py`

## Heap profiling

For investigating memory usage, allocation patterns, and leaks, Raven supports two heap profiling approaches.

### Quick RSS profiling with `analysis-stats`

The built-in `analysis-stats` command reports peak RSS after each analysis phase — useful for a quick check without any extra tooling:

```bash
cargo build --release -p raven
./target/release/raven analysis-stats /path/to/workspace
```

This reports timing and peak RSS per phase (parse, metadata, scope, packages). Use `--csv` for machine-readable output or `--only <phase>` to isolate a single phase.


## Releasing

### Version bump and tag

Use the bump script to update versions, commit, tag, and push:

```bash
./scripts/bump-version.sh           # patch bump (default): 0.1.0 -> 0.1.1
./scripts/bump-version.sh patch     # same as above
./scripts/bump-version.sh minor     # 0.1.1 -> 0.2.0
./scripts/bump-version.sh major     # 0.2.0 -> 1.0.0
./scripts/bump-version.sh 2.0.0     # explicit version
```

This updates `Cargo.toml` (workspace version) and `editors/vscode/package.json`, commits the change, creates a git tag, and pushes both.

### CI build

Pushing the tag triggers `release-build.yml`, which:
1. Cross-compiles the `raven` binary for 6 platforms (linux-x64, linux-arm64, macos-arm64, macos-x64, windows-x64, windows-arm64)
2. Packages a platform-specific `.vsix` for each target (the binary is embedded in `bin/`)
3. Generates a Sigstore-backed build-provenance attestation (`actions/attest-build-provenance`) for each `raven-<platform>.zip` and `.vsix`, so a published artifact can be verified with `gh attestation verify <file> --repo jbearak/raven`

### Publishing

After the build succeeds, manually run `release-publish.yml` from GitHub Actions:
1. Enter the tag (e.g., `v0.2.0`)
2. Optionally check "Publish to VS Code Marketplace and Open VSX"

This creates a GitHub Release with all binaries and `.vsix` files. If marketplace publishing is enabled, it uploads each platform `.vsix` to both VS Code Marketplace and Open VSX.

**Required `marketplace` environment secrets** (for marketplace publishing): `VSCE_PAT`, `OVSX_PAT`.

Optional post-release packaging jobs are gated by repository variables:

- `ENABLE_HOMEBREW_BUMP=true` opens a PR against `jbearak/homebrew-raven`. This requires `release` environment secret `HOMEBREW_TAP_TOKEN`.
- `ENABLE_APT_BUMP=true` builds Debian packages from the Linux release artifacts and opens a PR against `jbearak/apt-raven`, a GitHub Pages-backed apt repository. This requires `release` environment secrets `APT_REPO_TOKEN` and `APT_REPO_GPG_PRIVATE_KEY`, plus optional `APT_REPO_GPG_PASSPHRASE`. The apt repository stores signed `dists/stable` metadata plus `pool/main/r/raven/*.deb`; `names.db` is still not bundled and remains user-provisioned with `raven packages update`, `fetch`, or a committed `.raven/packages.json`.

## Cross-file internals (high-level)

Cross-file awareness is implemented under `crates/raven/src/cross_file/`.

Rough pipeline:
1. Extract per-file metadata (directives + AST-detected `source()` / source-batch requests + library calls)
2. Enrich inherited working directories and resolve `system.file()` sources
3. Discover Shiny application roles and expand filesystem-backed source batches off-lock
4. Compute per-file artifacts (exported interface, ordered timeline, interface hash)
5. Update dependency graph edges (forward + backward)
6. Resolve scope at a position by traversing edges and applying call-site filtering
7. Revalidate dependents on relevant changes (interface hash / edges / working directory changes)

Source-batch expansion is caller-owned and filesystem-backed; it has no
process-global cache. A finalized record stores its ordered ordinal sources and
every existing or potential watch root. `WorldState` derives a bidirectional
parent/root registry only after successful analysis commits. Ordinary commits
do not rebuild that registry eagerly: after the commit CAS succeeds, a bounded
gate re-reads only the parents whose open/closed authority changed and compares
their normalized roots with the installed parent map. The common `OpenEdit`
checks one parent and is therefore O(1) in workspace size. Only an actual
topology mismatch falls back to the full registry sweep, which remains the
single writer of both map directions, net owner-set generation bumps, and
removed-root tombstones. Artifact-cache installs and Pending admission include
any implicit LRU eviction victim in that bounded parent set; a claim-time
eviction is reconciled immediately because enrichment may later abort. Full
workspace replacements rebuild directly. Rejected commits never reach either
path. Watcher delivery
first advances a global monotonic event generation, closing the new-request /
pre-registry race, then advances the monotonic identities of overlapping roots
and looks up their parents. An affected open parent receives a per-URI
latest-arrival refresh owner on the backend's tracked routing-task lifecycle:
newer events update one desired-generation register while one coordinator
serializes physical filesystem walks for that parent. Exact-basis conflicts
retry with bounded backoff until commit, close/reopen, or shutdown. Coordinator
retirement releases single-flight admission before re-reading desired work, so
an event published during the exit window is either reclaimed or owned by a
successor. This prevents both consumed-event loss during unrelated graph/config
churn and bursts of uncancellable same-tree walks. `AnalysisBasis` validates
both the global fence and
the subject parent's captured root identities at the same write-lock commit
that installs metadata, artifacts, graph edges, and the rebuilt registry.
Removed roots remain generation tombstones; workspace replacement never resets
or reuses an identity. Ordered execution is represented by one `SourceBatch`
timeline event, while dependency edges retain individual ordinals so a member's
backward prefix contains exactly its earlier siblings. Batch members bypass
URI-only forward/standalone caches because their rolling symbol, removal,
package, and working-directory inputs are execution-specific. Descendant
ordinary sources carry a two-valued "prefer supplied `PathContext`" resolution
mode, never a call-site/ordinal occurrence identity; the forward-child memo
keys that mode alongside the context fingerprint. This preserves scope for
repeated members without multiplying graph or prefix-cache identities. The
graph remains URI-global, so navigation, missing-file checks, and revalidation
for nested context-dependent targets deliberately use its single winning edge.
For `raven check`, a pure bounded collector walks `(provider URI,
PathContext, lexical-first mode)` states from finalized tar members. The CLI
materializes any external providers through the normal
parse/enrich/`system.file()`/tar-finalize/artifact pipeline into its sole
`WorkspaceIndex`, then passes only providers outside the graph neighborhood to
the diagnostic snapshot's content-precollection seam. Context divergence or
budget truncation disables the standalone-scope cache for that snapshot, but
the collector never mutates graph edges or revisions.

Package-state seeds follow the same snapshot/off-lock/install discipline as
cross-file diagnostics. Callers snapshot the exact raw/derived/config/root
basis and open-record tokens under the `WorldState` lock, then capture a sorted
filesystem projection for DESCRIPTION/NAMESPACE, tracked package directories,
and the complete `.Rprofile`/testthat source closure. Each observed path is
Missing, Invalid, or Valid with the exact bytes/text consumed by detached
derivation; recursive membership catches additions, removals, and renames.
Open-authoritative inputs bind their record token and are omitted from the
shadow-disk projection. Missing/invalid/unknown closure targets never fall back
to a later disk read. The projection is checked after derivation and immediately
before the shared package-projection CAS. Contention gets three complete
attempts; obsolete root/config work is typed Superseded, stable exhaustion is
typed Deferred and coalesces into one exact-owner follow-up. Disabled modeling
choices are represented by empty scans before construction, and no filesystem
work occurs under the `WorldState` lock. After installation, callers refresh
open preamble documents to close the snapshot-to-install race.
Those silent refreshes return exact open-record candidates captured by the
write-lock apply itself; orchestration carries the tokens to finalization rather
than recapturing URIs later, so a close/reopen between refresh and publication
cannot transfer diagnostics ownership to the new lifecycle.

Background workspace scans use a process-wide latest-arrival intent and a
complete immutable candidate rather than merging a raw disk scan at commit
time. Each attempt captures its pre-I/O inputs, performs the disk scan, then
captures the exact post-I/O authority used to derive retained non-scan entries,
open-buffer overlays, package/system-file routing, metadata, and the complete
dependency graph off-lock. The central `PreparedAnalysisCommit::WorkspaceScan`
write-lock boundary preflights both bases and every exact open-record token,
then installs the index, graph, refreshed open metadata/artifacts, completion
state, and authority generations as one transaction. The index replacement is
an exact-version CAS and is the only fallible operation after preflight, so a
stale candidate mutates no central tier. Contention permits exactly one second
full scan-and-derive attempt under the same intent; a newer arrival supersedes
the old intent immediately, and exhaustion never falls back to a partial or
no-scan commit.

Diagnostics are deliberately not marked inside analysis transactions. Workspace
scan and system-file commits return exact one-shot transfer handles containing
post-commit open-record tokens. Startup, configuration, package, libpath, and
manifest orchestration finalize all handles at one shared boundary after their
collect-only convergence work. Finalization prevalidates every handle before
consuming any, unions and deduplicates candidates, applies the trigger cap, and
atomically creates reservations plus force-republish markers exactly once.
Overlapping handles therefore cannot double-mark a URI. A successful newer
commit inherits an unclaimed predecessor before recording a typed successor;
the predecessor is rejected with proof naming that successor, while failed or
pending attempts leave the predecessor claimable. Fallback uses the same
finalization identity and is idempotent, but is permitted only for a genuinely
missing/wrong owner. A handle already consumed by another finalization is a
terminal no-op for that handle: it is removed from a mixed handoff while every
remaining handle is still prevalidated all-or-none. Treating the consumed
handle as fallback authority would double-mark candidates that the first
finalization already owns. The consumed-owner ledger retains only triggers that
survived exact-record filtering and the cap; a later mixed handoff excludes
those still-current triggers only from remaining transfer handles, while
independent direct candidates may represent later semantic work on the same
lifecycle and still reserve. Cap-dropped and reopened-lifecycle candidates also
reserve normally. Watched batches likewise
exclude a pre-reserved candidate from a transfer only when the ticket trigger
still names the current open lifecycle; URI equality alone must not suppress a
close/reopen.

Workspace-scan finalization additionally validates the latest committed scan
intent. Each candidate validates its current diagnostic lifecycle and exact
open-record token, dropping closed, reopened, or otherwise replaced records.
Ordinary closed/open mutations may advance scan-input authority while a
transfer waits; only a successful newer transfer supersedes it.
Keep new scan inputs in the shared two-phase basis so startup, exclusion
reloads, and package-mode rebuilds cannot drift or introduce blocking
filesystem work under the state lock.

Document analysis has one authority per lifecycle state. Open buffers live in
`OpenDocumentStore`; all closed records live in `WorkspaceIndex`. The closed
authority has two residency tiers under one lock: metadata/scope artifacts
(default capacity 5000) and full Rope/tree payloads (default capacity 1000).
Full residency always implies artifact residency; full eviction leaves a
Complete artifact record, while artifact eviction also removes any matching
full payload. One shared pin set protects both tiers.

Each URI is explicitly Pending or Complete. On-demand workers atomically claim
a never-reused Pending generation before disk I/O and only that generation may
commit or abort. A full-record commit derives the artifact projection from the
same `IndexEntry`, preserving shared `Arc` identity. Workspace-scan ownership is
stored as record provenance (`WorkspaceScan { generation }` versus `Dynamic`);
there is no parallel URI side set. Scan replacement installs both tiers, pins,
and one version increment under the authority lock, retaining Dynamic
artifact-only entries. CLI per-file overlays project an indexed payload without
rereading or reparsing the file.

Detached closed-file analysis commits through
`WorldState::try_commit_analysis`. Its `AnalysisBasis` captures the exact
subject record plus every authority consumed while preparing the result:
watched generation, graph/config/package routing inputs, open-document context,
and bounded exact closed/raw context identities. The write-lock commit validates
the whole basis before mutating the index, raw cache, graph, metadata cache, or
fanout state. Rejection is otherwise a no-op; the sole exception is releasing
an exact still-current Pending lease so on-demand work remains retryable.
Watched-file groups use `ClosedBatch`, which rejects duplicate or stale members
before the first mutation, then applies the group with one pin recomputation and
one deduplicated, activity-capped fanout reservation. Before commit, peer
upserts/removals are overlaid on every Complete artifact resident (using the
full payload when resident and an empty-content projection after full-payload
eviction) and passed through the same bounded deterministic graph-derivation
fixpoint as workspace scans. Thus a changed child never commits metadata
derived from the pre-batch version of a parent changed or removed in the same
watcher group. Authority rejection permits one fresh reread/rederive retry.

Open-buffer edits, metadata refreshes, and `didOpen` installation use the same
`WorldState::try_commit_analysis` boundary. Their basis includes the exact
open-record generation and provenance, editor/lifecycle eligibility,
closed-index and raw-cache generations, and bounded alias/context identities.
Every family also captures the typed `AnalysisConfigGeneration` advanced by
the sole parsed-config writer after a complete recompute. This closes the
interval before later asynchronous reconciliation: depth/budget, on-demand,
package-enable, debounce, exclusion, and other parsed-analysis changes reject
old work immediately. Workspace folders and per-file chunk classification
retain their separate exact stamps.
Parsing and inherited-context derivation may run off-lock, but document,
metadata, graph, cache, pin, package, and fanout effects become visible only
after one write-lock validation. If an edit's context set exceeds the internal
hard ceiling, Raven keeps the exact prepared buffer and commits a fail-closed
local projection; one context-only retry is allowed when unrelated authority
changes, but a changed target token or a second invalidation discards the work.
Every dependent ticket returned by the commit owns a force-republish marker and
must be scheduled exactly once.

Package projections follow the same authority boundary. `PackageInputs` and
the derived `PackageState` are installed through one `WorldState` seam that
advances both record generations, refreshes the local-dev overlay, and mints a
new `system.file()` routing owner only when the effective package name/root
changes. Full seed/reseed installation deliberately mints a fresh routing owner
even for value-equal routing so detached convergence cannot attach to a prior
seed lifecycle. `OpenInstall` and `OpenClose` derive their complete package
projection off-lock and install it only after the surrounding open-analysis
basis passes preflight. `OpenEdit` does the same for live `.Rprofile`, an
already-known Rprofile sourced helper, and testthat preamble/source-closure
edits: it overlays the exact prepared Rope, scans both source-following inputs
off-lock, batches those deltas with any ordinary package-file event, derives
once, and installs the complete projection with the document and graph CAS.
Every source-following projection used by open, edit, close, or post-seed work
is captured without scan-time disk fallback: Raven records a byte-hashed
Valid, Missing, or Invalid observation for each disk-backed root/helper reached
by the iterative literal-source closure, then rechecks every observation
immediately before the state CAS. A rewrite, delete, recreate, or encoding
repair therefore rebases the whole projection rather than combining helper
bytes observed at different filesystem moments.
Package state therefore cannot become visible without the matching document,
graph, lifecycle, and fanout transaction. When any of those projections
changes routing, the open commit returns its exact routing owner plus unmarked
record/diagnostic candidates.
`didOpen`/`didChange`/`didClose` converge `system.file()` for that owner before
one shared cap-and-reservation step unions the routing transfer with the outer
fanout. Candidates retain their outer reservation policy through that union:
the open/edit subject wins the cap, keeps its captured debounce, and is not
force-marked, while dependent/system candidates keep dependent priority,
debounce, and force ownership. Retry exhaustion transfers those same exact
identities and policies to delayed work, so closing and reopening a URI cannot
redirect an old transaction's marker into the new lifecycle.

After a detached package seed installs, Rprofile and testthat preamble replay
also form one projection rather than two tails. One package/open-context basis
owns both scans and the single CAS, so observers see old/old before it and
new/new afterward. Three rejected whole-pair attempts return inert deferred
work keyed by the exact seed identity; no worker starts at that point.
Orchestration first registers the post-seed owner, its complete outer
handle/candidate bundle, and any exact pending `system.file()` dependency under
one write lock, then starts the workers. Registration distinguishes a new
owner, the same owner, and a different owner without replacing either ledger;
same-owner callers deposit their complete bundle, while different-owner
callers retain theirs and retry after the predecessor completes. An unrelated
authoritative live package edit does not silently cancel the tail: the
coordinator recaptures the whole pair against the current package basis while
retaining the outer seed/system handles and exact diagnostic candidates. A
different never-reused seed owner or root is a terminal handoff boundary: the
predecessor still consumes its retained ledger exactly once, while the
successor caller owns current-basis convergence. Package configuration and
library generations are broader: they invalidate the exact projection, but
rebase against the current basis because not every such change creates a
successor seed caller. The package-projection basis carries an explicit
foreground or
exact-coordinator ownership expectation, so registration or routing ownership
that changes after a tail capture rejects the whole projection at the central
CAS. A coordinator whose registration requires routing convergence cannot
commit until that exact transfer is deposited in its retained ledger. A
root/config successor is terminal for that pair but still
consumes the retained outer ledger exactly once; a later seed fails its install
CAS as a strict no-op and transfers to the coalesced retry lifecycle while the
predecessor coordinator is pending, so it cannot overwrite and orphan that
ledger. The coordinator waits for every registered routing dependency, unions
the stable tail candidates with every retained handle, and applies the cap,
reservations, and force markers in one finalization. Deferred `system.file()`
convergence transfers its routing handle to this coordinator rather than
independently finalizing or fresh-capturing open URIs.

`didOpen` additionally owns a never-reused per-URI intent generation, whose
arrival-time target record cannot rebase across an edit, close/reopen, metadata
replacement, or newer duplicate open. It resolves alias topology off-lock,
validates the intent/target/topology under the diagnostics-publish/write-lock
commit order, and only then mints the diagnostics epoch and installs the
record, aliases, lifecycle, raw-cache invalidations, graph, pins, package
projection, and final capped reservation. Direct/forward/backward
prerequisites converge before that commit: dynamic Pending enrichment reports
unmarked affected candidates, any already-owned tickets are drained, and a
fresh final capture reserves the complete union once. `.Rprofile` and testthat
scans overlay all authoritative buffers plus only package routes the candidate
will authoritatively own, and reduce with its package `DidOpen` event into one
package state. Package-library initialization precedes every derivation that
can resolve `system.file()`. Each on-demand library build owns an exact key
(R path, additional paths, workspace root, and package-input generation) plus
the package-config generation and not-ready state it observed. Its final swap
is a CAS on that basis; stale builds are discarded and `didOpen` performs one
bounded fresh-key retry. A stable degraded/unavailable build is remembered by
its winning key so opening continues fail-closed with unresolved package
sources instead of looping. After commit, direct and converged-scope inherited
packages are warmed synchronously before any diagnostic ticket starts. A
bounded convergence overflow clears foreign Rprofile/preamble provenance and
commits only candidate-local facts; there is no delayed stabilization publish.
The shadowed closed artifact tier remains resident while the buffer is open:
open-provider precedence hides it, closed commits still veto, and close can
fall back to it without a transient empty-scope window before disk resync.

`didClose` is the corresponding first-class `OpenClose` transaction. Its
never-reused intent is bound to the arrival-time open record, lifecycle epoch,
alias ownership, configuration, graph, closed-index, raw-cache, and package
inputs; retries may refresh ancillary context once but never rebase onto an
edit, metadata replacement, duplicate open, or close/reopen lifecycle. Disk
roots are read and parsed off-lock, represented by exact content-hashed
snapshots, and revalidated while the diagnostics publish lock is held
immediately before the central state CAS. The commit atomically removes the
open record and lifecycle, hands aliases to surviving owners, installs a
retained shadow or validated fresh-disk projection (or removes a
missing/excluded root), updates graph/cache/package/Rprofile/testthat state,
refreshes pins, claims watched generations, and reserves one capped fanout.
The same selected projection feeds every subsystem. The empty diagnostics
publication completes while the publish lock remains held; only afterward are
immutable dependent tickets and retained-shadow disk convergence released.
Close-resync rereads and revalidates its disk snapshot before its own CAS, so a
reopen or intervening write vetoes stale work.

Watcher events on an exact open client URI also drive bounded alias-topology
reconciliation. Alias candidates and graph mirrors are derived off-lock from
an exact open-generation/config/owner basis; one commit swaps the alias map,
invalidates old and new raw identities, hands released canonical roots to the
next open owner, refreshes pins, and reserves one capped fanout. A released
root with no remaining owner is immediately resynced to disk. Canonical-target
events do not mutate alias topology: only the uniquely keyed opened spelling
can have been retargeted, so one event affects at most one open record.

The package-state-local static-source closure walker in `package_state/mod.rs`
owns the traversal mechanics shared by `.Rprofile` and testthat preambles:
depth-first execution frames, rich forward path resolution, active-stack cycle
rejection, and the common depth/file/replay budgets. Caller policies only decide
root harvesting, open-buffer text overrides, exclusions, and the `.Rprofile`
`renv/activate.R` exception. Accepted
missing or unreadable targets remain in the routing closure so a later create or
recreate watcher event can rescan them.

### Module map

Key files under `crates/raven/src/cross_file/`:

- `mod.rs` — Public API (`extract_metadata()`)
- `dependency.rs` — Dependency graph (forward + backward edges)
- `scope.rs` — Scope artifacts, interface hash, scope-at-position resolution
- `path_resolve.rs` — Path resolution (forward vs backward invariant)
- `revalidation.rs` — Real-time updates, diagnostics gating, debounce
- `source_detect.rs` — AST-based detection of `source()`, `sys.source()`, and related calls
- `binding.rs` — Shared conservative binding-count and `assign()` argument-matching walk used by static path and package-vector inference. Its bounded bare-helper policy recognizes exact local shadowing rather than attempting runtime search-path resolution; the `readLines` direct-effect classification uses a whole-document precheck so later or nested recognized bindings cannot make results traversal-order-dependent. Eager `for` nodes expose a pre-effect checkpoint before installing their unknown-mutation barrier, allowing deterministic loop consumers to snapshot an immutable shared package vector while every post-loop query still sees the barrier; pending persistent invalidations suppress the checkpoint
- `static_path.rs` — Strict folding of computed `source()` path expressions
- `directive.rs` — Parsing of `# raven:` / `@lsp-` directive comments
- `types.rs` — Shared types (`CrossFileMetadata`, `SymbolKind`, etc.)
- `config.rs` — Cross-file configuration (cache sizes, debounce intervals, feature flags)
- `parent_resolve.rs` — Parent-prefix scope resolution (backward-edge traversal)
- `cache.rs` — `MetadataCache`
- `file_cache.rs` — `CrossFileFileCache` (content + existence)
- `property_tests.rs` / `integration_tests.rs` — Property-based and integration test suites

The open-doc-authoritative provider and the closed two-tier authority live at
`crates/raven/src/content_provider.rs` and
`crates/raven/src/workspace_index.rs`, respectively.

### box::use module imports (#662)

box (`box::use()`) is modelled as a **selective import** — a third kind of
cross-file relationship, distinct from both a `library()` package load
(`ScopeEvent::PackageLoad`) and an ordinary lending `source()` edge. Two crate
modules own it, split by concern:

- `crates/raven/src/selective_import.rs` — the **syntax-agnostic** data model.
  `SelectiveImportRequest` carries an `ImportSource` (a package name, or a
  dialect-bearing resolved local-module identity), optional `NamespaceBinding`,
  `AttachBinding`s, destination, exclusions, and import provenance. Resolution combines it with an `ExportSet` carrying an
  `ExportCompleteness` marker and per-member provenance. It is deliberately
  independent of surface parsing and backs both box and `{import}` (#663). Its
  interface hash folds semantic import/export boundaries,
  not source-line provenance, so moving an unchanged import does not churn
  dependents.
- `crates/raven/src/box_use/` — everything **box-specific**:
  - `detect.rs` parses `box::use()` / `box:::use()` calls. Specs are parsed from
    the raw argument text, not only the argument sub-tree, because R precedence
    makes `./mod[a, b]` parse as `. / (mod[a, b])` (`[` binds tighter than `/`).
  - `exports.rs` parses `box::export()` unions and `#' @export` tags, including
    re-exported imports and symbolic wildcard re-exports. Marker-less modules
    derive their partial legacy fallback from the canonical live top-level
    scope rather than from a separate definition scanner.
  - `path.rs` resolves a local `BoxSpec::LocalModule` without reusing
    `path_resolve.rs`: box paths are file-relative, ignore `# raven: cd` /
    testthat WD / workspace-root fallback, are case-sensitive, and try
    `path.r`, `path.R`, `path/__init__.r`, `path/__init__.R`. The resolved URI
    stays raw/non-canonicalised, matching Raven's URI identity convention.
  - `resolve.rs` combines explicit or legacy module exports with package
    snapshots, resolves recursive named/renamed/wildcard re-exports with cycle
    and depth guards, and preserves exact original-definition provenance.
- `crates/raven/src/import_pkg/` owns `{import}` detection, exact literal-script
  path lowering, and its separate partial live-private-environment export
  policy. `CrossFileMetadata::selective_import_requests` is the centralized
  lowering iterator used by scope and graph code. Named destinations stay in a
  lower-priority layer in both recursive and streaming scope; they are never
  namespace aliases.

`CrossFileMetadata` stores `box_imports: Vec<BoxImport>` and
`box_exports: Option<BoxExports>` (both `#[serde(default)]`). Local imports also
persist their detached `LocalModuleResolution` (`Resolved`, `CaseMismatch`, or
`Missing`). Syntax extraction leaves that field unset; open/edit, workspace
scan, watched resync, on-demand indexing, excluded-file, and CLI preparation
seams enrich it only after dropping any `WorldState` guard. Graph construction,
scope/artifact resolution, diagnostics, navigation, references, and hover
consume the persisted outcome and must never probe the filesystem themselves.
Watched candidate events re-enrich unchanged importers off-lock and guard an
open-record replacement by its never-reused analysis generation, so a stale
watch refresh cannot overwrite newer editor text.

`ScopeEvent::SelectiveImport` places resolved imports in the ordinary ordered
scope timeline; the same `SelectiveImportProvider` seam is used by recursive and
streaming resolution, diagnostics, interactive requests, and parameter
resolution. Namespace aliases retain source identity separately from their
visible symbol so shadowing and removal cannot leave stale member resolution.

Local module imports produce `DependencyEdgeKind::SelectiveModule`. These edges
are present in full and trimmed graphs and participate in interface-driven
revalidation, but `lends_scope()` is false and ordinary-source-only traversal
excludes them from parent-prefix lending and cross-file `# raven: nse` /
`# raven: func` propagation. This typed distinction must not be replaced with
`non_lending`, whose separate exclusion semantics preserve `S_trimmed ⊆ S_full`.

Package imports reuse `PackageLibrary::namespace_exports_snapshot_sync`; the
query is synchronous and never spawns R or mutates the cache. Completeness is
preserved, so only a `Complete` set proves a selected member absent. Static box
package references enter the existing package-warming lifecycle. Local module
exports and provenance come from canonical `ScopeArtifacts` and metadata, not a
parallel index.

Qualified member resolution for `$`, `@`, and literal `[[...]]` recognizes box
namespace aliases before ordinary structural-member lookup. Local definitions
navigate through re-export chains; package members deliberately have no file
location. Selective-member references are identity-filtered by repeatedly using
go-to-definition, while ordinary structural references retain their historical
name-based pooling behavior. Closed/indexed definition hover extracts statements
from `WorkspaceIndex` before falling back to the cross-file file cache.

### Path resolution invariant (forward vs backward)

There is a deliberate distinction in how relative paths are resolved:
- **Backward directives** (`# raven: sourced-by`, `# raven: run-by`, `# raven: included-by`) are resolved **relative to the child file’s directory**, ignoring `# raven: cd`.
- **Forward directives** (`# raven: source`, `# raven: run`, `# raven: include`) and AST-detected **`source()` calls** resolve using `# raven: cd` when present (otherwise file-relative).

Implementation:
- Backward directives must use `PathContext::new()`.
- Forward directives and `source()` calls must use `PathContext::from_metadata()`.

User-facing explanation/examples live in `docs/cross-file.md`.

### Workspace-root fallback for unannotated codebases

For codebases without `# raven: cd`, `source()` paths are often written relative to the workspace root.

For **AST-detected `source()` calls and forward directives** (`# raven: source`, `# raven: run`, `# raven: include`), Raven attempts:
1. File-relative resolution
2. If the file does not exist *and* there is no explicit/inherited working directory: try workspace-root-relative resolution

Forward directives are semantically equivalent to `source()` calls (see `.kiro/specs/lsp-source-directive/`) and must resolve identically across dependency edges, scope, diagnostics, cmd-click, and path completion. This fallback must **not** apply to backward directives.

### Case-leniency vs workspace-root fallback (decoupled, issue #535)

These are two **independent** concepts; do not re-conflate them:
- **Single-case-insensitive-match leniency** — fold a wrong-cased path to the unique on-disk match (exact wins; 2+ ambiguous stays unresolved; ASCII-only). This is **direction independent**: both forward `source()`/directives and backward directives (`# raven: sourced-by` etc.) get it, so a wrong-cased backward directive resolves on a case-sensitive filesystem instead of dropping the edge and cascading false `undefined-variable` warnings. Issue #530 introduced it forward-only; #535 made it bidirectional.
- **Workspace-root fallback** (above) — try the path relative to the workspace root. This stays **forward-only**.

In `resolve_path_rich`, the `try_workspace_fallback` bool now gates **only** the workspace-root fallback; the case-leniency branches are unconditional. Backward resolution (`resolve_path` / `resolve_backward_path_rich`) passes `try_workspace_fallback = false` — gaining the leniency but never the fallback. The backward case-mismatch diagnostic (`collect_backward_case_mismatch_diagnostics_standalone` in `handlers.rs`) reuses the `source-path-case-mismatch` code and `caseMismatchSeverity` policy but with a message that does not claim R errors (R never executes a backward directive).

### Parent-prefix scope and forward-source traversal

Scope resolution has two distinct graph traversals:
- **Parent prefix**: when resolving a file, Raven first walks backward edges to model symbols available at the start of that file. Symbols introduced only by this prefix are tracked separately so they can be filtered when a child is re-exported through a forward `source()` edge. `ForwardSource` and `DependencyEdge` carry `SourceLocality` end to end: `Global` and `CurrentFrame` lend ordinary bindings available through their global/call environments, while `NonInheriting` is declared-only and a hard package barrier. `CurrentFrame` is composed with evaluated capture frames before metadata is stored: caller remains current-frame, global promotes to global, and external/unknown becomes non-inheriting. `ForwardSource` stores only this enum at runtime; custom serde projects the legacy `local`/`sys_source_global_env` booleans on output and normalizes them on input for metadata compatibility. Its serde-defaulted `guarded_by_file_exists` bit is diagnostic-only: graph identity, scope, navigation, and revalidation ignore it, while the path collectors use it to suppress only absent/case-mismatched exact optional sources. Existing outside-workspace targets remain errors. Bare `file.exists` uses the project-wide bounded lexical helper policy: same-file exact/dynamic named bindings are considered, while package- and source-provided masking is deliberately outside this syntax-local proof; `base::file.exists` is unambiguous.
- **Forward sources**: Raven then applies the file's local timeline. When following `source()` calls, real forward-source execution uses a path-local copy of the visited map so one sourced sibling's recursive walk cannot make a later sibling source look already visited. Parent-prefix discovery keeps shared visited pruning to avoid expanding every possible parent/forward path while collecting inherited context. Non-inheriting targets retain graph edges for diagnostics and revalidation but never enter the lending timeline.

Ordered source batches share the detached expansion, watch-root ownership, generation fencing, graph identity, and left-to-right scope resolver originally built for `{targets}` `tar_source()`. `SourceBatchKind` separates tar's contextual working-directory semantics, ordinary bounded `list.files()` source loops, legacy Shiny global loading, and Shiny's shared helper support environment. The serialized `tar_source_ordinal` and `tar_source_expansion_watch_paths` names remain for metadata compatibility, but their runtime contract is batch-generic: kind plus call site plus ordinal identify a member, and expansions replace all derived members atomically. The ordinal name was reassessed after adding Shiny batches: no semantic change or rename is warranted because ordinals remain dense, zero-based positions within every ordered batch, while renaming the serialized field would add compatibility churn without clarifying runtime behavior. Detection never touches the filesystem; expansion runs only after working-directory enrichment and outside state locks. Request-bearing `didChange` operations enumerate inside the existing detached OpenEdit derivation and await its atomic document/metadata/graph commit. If any source-file event in one normalized watcher notification belongs to an open source-batch parent, every normalized source-file event in that notification runs as one invocation-owned watched transaction (including any package projection). Config-layer mutations are applied first, but their diagnostic publication joins the same completion boundary. This prevents the parent from publishing after its batch member lands but before an ordinary dependency event from the same notification commits. The owned transaction drains its diagnostic reservations before completing the receipt-backed parent refresh; if package convergence crosses the existing coalesced retry boundary, the parent-refresh obligation moves with that owner and the original handler awaits the shared terminal receipt. A cancelled parent refresh cancels the outer receipt and withholds sysdata admission. Sysdata continuation otherwise keeps the ordinary watcher contract: admission is ordered after the owned transaction, but convergence remains lifecycle-owned. Workspace-replay refreshes use the same generation-owned parent receipts. Workspace graph derivations reuse the expansion already owned by the scan candidate, keeping the complete-graph pin pass and final graph pass on one filesystem observation. Each expansion records the project exclusion patterns used to select its members; reuse fails and re-finalizes both open and closed metadata when those patterns change. Unsafe enumeration and member-cap overflow fail closed without publishing a partial batch; traversal-budget overflow uses the deterministic ordered fallback described below.

Bounded `list.files()` expansion retains a typed internal failure reason for member-cap overflow, an unreadable directory entry or matching file, and a matching `.R` directory. The outer request expansion emits exactly one debug record containing the parent, call site, directory, and reason while preserving the existing empty-batch result. Warning-level logging was rejected because open/edit/scan/watcher refreshes can repeat the same transient filesystem condition. Editor hints were also rejected for this release gate: expansion has several detached owners and no stable diagnostic lifecycle for these failures, so a correct hint would require deduplication keyed by parent, call site, failure, and watch generation. That broader lifecycle remains deliberately out of scope.

The watcher regression suite includes a 32-round create/change/change/delete soak across a tar parent, a Shiny parent sharing the same `R/` member, and a bounded `list.files()` parent. Every mixed notification must advance the global event fence and both watched-root generations, recompute all three parents, converge metadata/graph/artifacts/undefined-symbol diagnostics, release coordinator/completion/revalidation/coherence owners, and return transient registry and closed-artifact cardinalities to their bounded baseline. The test also verifies that deliberate generation tombstones stop growing after the first indexed Shiny helper establishes its own transient watch roots. Current production admission serializes watcher notifications and provides no reproducible path for overlapping independent coherence receipts, so receipt redesign remains deferred. Revalidation also found no current-main regression requiring `lapply`/`sapply`/`purrr::walk` detection or alternate regex equivalence; those detector broadenings remain unsupported rather than being added speculatively.

Shiny layout discovery is an enrichment-time filesystem operation in
`cross_file/shiny.rs`; scope queries never rediscover mode or membership. The
candidate gate is intentionally narrow, while discovery reads only the direct
application directory and exact direct `R/` directory. The captured project
exclusion matcher filters the current candidate and directory entries before any
mode, marker, ordinal, or batch decision. Selection is case-insensitive with
exact-spelling precedence and ambiguity failing closed; `server.R` selects
legacy mode before `app.R` is considered. Active entries receive pre-entry
source batches: legacy `global.R` is a one-member global batch, then helpers
form an ordered shared-support batch. Single-file mode omits the global batch.
Candidate metadata survives incomplete layouts and inactive conventional files
so watcher ownership can observe entry creation and mode transitions. Candidate
roles remain semantically ordinary: they neither join application identity and
declaration filtering nor receive the application working directory. Semantic
identity for active members is canonical, while `application_working_directory`
keeps the lexical application root; lexical and canonical watch roots map back
to the one raw parent URI.
Discovery also retains the exact selected entry transiently. When a helper or
legacy global is opened before workspace indexing, the existing open-install
prerequisite loop indexes that host and its forward batch, recaptures the
analysis basis, and only then commits the open document and starts diagnostics.

The scope resolver treats pre-entry batches as explicit environment stages.
Legacy global contributions seed support; helper top-level evaluation receives
only its ordered prefix; entry roots receive completed support. Function bodies
inside helpers resolve against completed support to model R's late binding.
Helper removals are layer-local: removing a support shadow reveals an inherited
global binding, while removing a support-only name leaves it absent. `ui.R` and
`server.R` remain sibling environments and never lend entry-local bindings to
one another. Cross-file `# raven: nse` / `# raven: func` collection applies the
same roles in two phases: eager policies use only preceding helper declarations,
while deferred function-body policies may use the completed helper set.
Revalidation deliberately remains a conservative graph superset.

`application_working_directory` is derived during the same enrichment and sits
between explicit `# raven: cd` and inherited working-directory state in
`PathContext`. It is a hard forward-resolution base, suppresses the workspace
fallback, and propagates through ordinary nested sources until `chdir = TRUE`
reanchors at the child directory. Backward `PathContext::new` remains unchanged.
The generic source-batch root registry owns Shiny entry/global/helper/marker
refreshes, so watched mode changes reuse the same generation fences, open-buffer
authority, closed-file indexing, atomic graph commit, and diagnostic receipt
boundary as tar/list batches. Registry overlap is recursively conservative, so
Shiny owner selection refines it with an ASCII-case-insensitive direct-child
check: incomplete candidates react only to `server.R` / `app.R` mode selection,
while selected layouts also react to direct `ui.R`, `global.R`, `R/`, helper, and
disable-marker events. The shared component comparator also equates normal and
extended-length Windows disk/UNC prefixes, so canonical symlink roots match
ordinary file-URI events. Unrelated application-root descendants therefore stay
in the ordinary watcher transaction.

`tar_option_set(packages = ...)` uses a distinct durable metadata channel,
`CrossFileMetadata::targets_pipeline_packages`, rather than synthetic
line-zero `PackageLoad` events. Detection and static-vector resolution happen
once during metadata extraction; query-time scope resolution never reparses the
declaring file. Recursive and streaming resolution project the package-name set
onto the declaring pipeline only after its ordinary timeline, making the set
order-independent without feeding it into ordinary `Source` events. A private
TarSource initial scope is seeded with the same set before ordinal zero executes;
ListFiles batches are not seeded. Targets-only loaded/attached provenance markers
survive parent-prefix and ordered-batch merging so TarSource edges can lend the
contribution while ordinary Source/ListFiles edges filter it. A real lexical
package load clears the corresponding marker, preserving normal propagation
when both channels name the same package. Conditional helper classification
(such as Shiny deferred scopes) reads the stored targets set directly without
installing it in the running frames, maintaining the same propagation barrier.
The interface hash includes the sorted, deduplicated package-name set: package
order and source anchors are hash-neutral, while a semantic set change
revalidates dependents.

The cached/streaming parent prefix is seeded with the queried URI at `(u32::MAX, u32::MAX)` so a single cache slot covers all positions in that file within a snapshot, whereas the uncached point resolver (`scope_at_position_with_graph`) seeds the visited map at the real `(line, column)`. For acyclic graphs the two seeds are identical. In a cyclic graph the cached prefix is a deliberate over-approximation: a back-edge can re-enter the queried file at `(MAX, MAX)` and so could "see" symbols defined later in that file, but the same-file leak filter (a symbol whose `source_uri` equals the queried URI is dropped from a parent prefix) plus `entry().or_insert()` merging keep the result sound at the symbol level, so those later symbols never surface. Tightening this would require position-dependent cache keys, which would defeat the cache; it is therefore pinned as intentional — `test_cyclic_cached_overapproximates_vs_uncached_point_query` observes the cached and uncached resolvers agreeing on every field at the boundary query (issue #374).

### Interface hash + selective invalidation

`ScopeArtifacts` includes an `interface_hash` used to avoid cascading revalidation when edits don’t change a file’s “exported interface”.

The hash is deterministic and includes:
- Exported symbols — each hashed through `impl Hash for ScopedSymbol`, which covers that type's full identity field set (`name`, `kind`, `source_uri`, `defined_line`, `defined_column`, `signature`, `is_declared`) and deliberately excludes only `defined_end_column` (positional highlight metadata). `Hash` must mirror `PartialEq` field-for-field: a field present in one but not the other makes the hash blind to that kind of edit. The `signature` inclusion (issue #482) is what makes a formals-only edit (`f <- function(a)` → `f <- function(a, b)`) bust the hash and revalidate dependents; `scoped_symbol_hash_eq_field_parity` pins the two impls' field sets together.
- Loaded packages (from `library()` / `require()` / `loadNamespace()` and
  static `pacman::p_load()` calls), including whether each event actually
  attaches the package and any attachment prerequisite on a conditional bare
  helper call. Scope results retain a separate attached-package fact because
  `loadNamespace()` remains available for namespace-aware analysis but must not
  enable attachment-sensitive helpers.
- The semantic package-name set from `tar_option_set(packages = ...)`, sorted
  and deduplicated so declaration position and order do not cause needless
  dependent revalidation.
- Declared symbols (from `# raven: var` / `# raven: func` directives)
- Source-edge inputs, including precise `SourceLocality`, call site, path, `chdir`, function scope, and resolved URI. `CurrentFrame` and `NonInheriting` remain distinct runtime/hash values even though old serialized readers see the same lossy `local = true` compatibility projection for regular `source()` calls.

Conditional or inherited package state is also kept out of Raven's immutable
function-scope tree. A bare Shiny deferred helper whose attachment is not
provable from preceding local loads is recorded as a sidecar candidate, not an
unconditional `FunctionScope` event. Recursive and streaming resolution
activate that candidate only when their ordered attached-package projection
contains `shiny` immediately before the helper call. The projection interleaves
source/tar contributions with package events, retains the owning recursion
depth and ancestor cycle guard, and keys reused source contributions by the
inherited attachment set. Inside a function it uses the completed top-level
projection (R late binding); at top level it never admits a later attachment.
This preserves the authoritative timeline for inactive calls while allowing a
later global attachment inside an enclosing function, conditional
`p_load(shiny)`, or an inherited attachment to enable the same deferred-body
isolation in both resolvers.

Package-state `.Rprofile` and test-preamble closure scans replay their static
top-level source and package events in execution order. Sourced children mutate
the same bounded attachment environment before the parent resumes, so a
preceding `library(pacman)` can enable a child's bare `p_load()` and a child
attachment can enable a later parent call. A completed routing path executes
again only after the monotonic environment gains a package required by a known
conditional call; duplicate source calls at the same attachment generation
collapse. Active-stack cycle rejection, the depth/distinct-file budgets, and a
fixed per-file replay multiplier are deterministic completion boundaries.
Missing or unreadable cap-boundary targets remain watcher-routed. Test preamble
roots share this environment in lexical order; incremental rescans replay later
roots only until the new cumulative environment converges with the prior one.

The backend uses `old_interface_hash` vs `new_interface_hash` to decide whether to revalidate dependents.

### Caching model

Cross-file caches use the `lru` crate and are wrapped with locks for interior mutability.

Concurrency choice:
- Reads use `peek()` (no LRU promotion) under read locks.
- Writes use `push()` under write locks.

Implication: eviction is effectively “LRU by insertion/update time”, not “LRU by access time”, which keeps the read path maximally concurrent.

Default capacities are defined close to each cache:
- `MetadataCache`: `crates/raven/src/cross_file/cache.rs`
- `CrossFileFileCache` (content + existence): `crates/raven/src/cross_file/file_cache.rs`
- `WorkspaceIndex` (artifact and full tiers): `crates/raven/src/workspace_index.rs`

Cache sizes are configurable via VS Code settings and applied during initialization/config change.

One cache lives outside the cross-file system: `compile_cached` in `crates/raven/src/linting/config.rs` memoizes compiled object-name regexes in a process-wide `Mutex<LruCache>` (cap 512, not configurable). It exists because per-document `[[linting.overrides]]` resolution re-parses the lint config on every debounced edit, which would otherwise recompile every configured pattern each time. The lock is exclusive but only config parsing takes it; the per-name `is_match` hot loop never touches the cache. See the function's doc comment for the full rationale.

### Self-contained isolated-scope cache (#483 / WI2b)

`# raven: self-contained` callees (alias: `# raven: standalone`; see `docs/cross-file-analysis-performance.md`) are resolved in isolation: no backward parent-prefix walk (WI2a) and - when resolved as a caller's forward `source()` child - canonical, caller-independent inputs (empty packages, no `data()` provider, the file's own `PathContext`; WI2b "part 2"). That makes a self-contained file's depth >= 1 isolated EOF scope a pure function of the file and its own forward `source()` closure plus the traversal config, independent of who sources it, so it is cached **across snapshots** rather than per-query - collapsing the per-caller, xN-revalidation-fan-out, and per-file-`raven check`-snapshot re-resolutions into one compute plus cheap `Arc` clones. The Rust module and type names still use `standalone` as the historical internal name for this self-contained-file optimization.

- Module: `crates/raven/src/cross_file/standalone_cache.rs` (`StandaloneScopeCache`, `StandaloneScopeKey`, `StandaloneCacheCtx`). Owned by `WorldState` behind an `Arc` so the diagnostics snapshot clones the handle out from under the read lock and consults the cache with no `WorldState` guard held; default capacity is in that module.
- Key = `(callee_uri, edge_revision, closure_interface_fingerprint, max_chain_depth, hoist_globals, backward_dep_mode, package_config_generation)`. `edge_revision` pins set membership and must be read from the **real** graph (a cloned per-snapshot `DependencyGraph` resets its own counter); `closure_interface_fingerprint` is an order-sensitive hash over the per-file `interface_hash` of C's **contributing set** - the forward `source()` closure of C plus the backward parents of any non-self-contained closure member (transitively; a non-self-contained member runs its own parent-prefix walk, so those parents' `library()`/defs feed C's scope), never following backward edges out of a self-contained file. So any contributing file's interface edit misses (body/comment/local edits don't). `package_config_generation` (bumped on package-library re-init) is defense-in-depth.
- Hook + soundness: consulted at one place - the top of `scope_at_position_with_graph_recursive`, gated on self-contained/standalone metadata + full EOF (`MAX,MAX`) + `current_depth >= 1` (the depth-0 own-root query injects `base_exports` and is excluded) + canonical inputs + acyclic graph + no closure member already in `visited` (a visited member would short-circuit its forward `source()` into a truncated, caller-path-dependent scope). Reuse mirrors `ForwardChildMemo`: store only truncation-free scopes, never under cancellation, and serve a stored `compute_depth` for a `reach_depth <= compute_depth` (keeping the deepest). A cache HIT is byte-identical to the un-memoized resolver; `RAVEN_DISABLE_STANDALONE_CACHE=1` forces every resolution to recompute so `raven check .` cache-on vs cache-off can be diffed (the #483 acceptance gate), and `raven check` logs hit/miss counts at `RUST_LOG=raven::cli=trace`.
- Benchmark: `bench_standalone_cache` (group `cross_file_standalone_cache`) in `crates/raven/benches/cross_file.rs` measures cold-miss vs warm-hit vs cache-off caller resolution, the fan-out aggregate (directive vs no-directive), and steady-state single-completion cost, on a deep/wide hub corpus shaped to mirror real high-fan-out workspaces. The single-completion case is the user-visible "completion menu appears faster" win in `docs/cross-file-analysis-performance.md`.

### Real-time diagnostics & monotonic publishing

Cross-file revalidation is debounced and cancelable.

A diagnostics gate enforces monotonic publishing:
- never publish diagnostics for an older document version
- allow “force republish” at the same version for dependency-triggered revalidation
- never let retired work publish into a reused URI lifecycle

The third rule is the per-URI **lifecycle epoch** (#603). Version and revision
cannot identify a lifecycle — a client may reopen at the same version, and
`Document::revision` restarts at 0 on every open, so a worker retired by a
close, tab removal, or shutdown could otherwise pass every freshness check
against the URI's next lifecycle. The gate mints a globally unique epoch when
a URI becomes diagnostic-eligible (`did_open`, or a tab re-addition after the
`editor_diagnostic_uris` replacement), retires it on `did_close`/tab
removal/shutdown, and refuses any commit whose captured epoch is no longer
current — without consuming a force marker. `begin_epoch` also resets the
URI's version/force state so a fresh lifecycle can neither inherit a stale
same-version high-water mark nor orphaned force markers. Both publish
pipelines carry the epoch inside `DiagnosticsTrigger` (`state.rs`), captured
when the work is spawned; the debounced path additionally declines to
`schedule()` on a stale or missing epoch so retired workers cannot supersede
the live lifecycle's pending worker. Lifecycle transitions go through
`WorldState::begin_diagnostic_lifecycle` / `retire_diagnostic_lifecycle` so
the cancel + gate-clear pairing cannot drift between call sites.

See `crates/raven/src/cross_file/revalidation.rs`.

For deterministic race tests, `WorldState::diagnostics_test_pause`
(test/test-support builds only) can park a diagnostics worker in the
post-compute, pre-commit window; see the `#603` regression tests in
`backend.rs`'s `diagnostic_lifecycle` module.

The VS Code extension sends an optional `diagnosticUris` set in both initialization options and `raven/activeDocumentsChanged`. Initialization seeds the policy before vscode-languageclient synchronizes already-open documents; the notification keeps it current. The set is the union of resource-backed tabs (the modified side for a diff tab) and visible text editors (to include peek editors). This is intentionally distinct from `workspace.textDocuments`: review/comment extensions can call `workspace.openTextDocument()` for background files, which sends LSP `didOpen` even though no editor tab exists. Raven keeps those documents synchronized for cross-file analysis but gates their own push diagnostics. An absent set preserves standard `didOpen` behavior for older and non-VS Code clients.

Diff editors expose both sides through `window.visibleTextEditors`, so the tab collector records diff originals and excludes them from the visible-editor union; an original that also has its own ordinary tab remains eligible. vscode-languageclient also retains its push-diagnostic collection across automatic crash restarts. The extension therefore listens for every transition to `State.Running`, waits for the current `start()` promise (the state event fires before initialization completes), then resends ownership and indentation state. The common activity path deletes retained diagnostic entries outside the current resource set before its running-state guard, so a tab closed during server downtime clears immediately and the post-restart reconciliation closes any initialization-snapshot race.

Eligibility is checked both before diagnostic computation and at the final atomic gate commit. `WorldState::diagnostics_publish_lock` serializes the final check + client send with tab-removal and `did_close` empty publications; do not remove that serialization in favor of cancellation alone, because a worker can otherwise pass its last state check, lose the clear race, and publish stale Problems afterward. A URI removed from the editor-resource set has pending revalidation cancelled and its diagnostics gate cleared; adding it back explicitly schedules diagnostics because a background text model that remained LSP-open will not send another `didOpen`.

Each `raven/activeDocumentsChanged` arrival owns a process-wide, never-reused
intent; the client timestamp is activity payload, not transaction ordering.
`WorldState::commit_open_lifecycle_batch_if_current` validates that intent and
atomically replaces the diagnostic-resource policy, retires removed epochs,
mints added epochs, and captures immutable added-work triggers from the same
commit. The handler sends every removal clear while still holding
`diagnostics_publish_lock`, then releases it before starting bounded work from
those exact triggers. Do not route the added set through the URI-only fanout:
its later fresh capture could attach delayed work to a remove/re-add lifecycle
that did not exist when the batch committed. An activity-only notification
uses the same arrival sequencer but changes only activity hints.

Raven intentionally keeps `tower-lsp` at `.concurrency_level(1)` so text-sync notifications remain ordered. Do not use global LSP concurrency to speed up diagnostics. Dependency-triggered fan-out is localized in `crates/raven/src/backend.rs`: `publish_diagnostics_for_uris_bounded` runs the normal debounced diagnostic pipeline for an already-computed URI set with a small fixed concurrency limit (`DIAGNOSTIC_FANOUT_CONCURRENCY`). Each worker rebuilds its own snapshot and commits through the monotonic gate.

### Closed-file disk resync and commit (#558, #650)

`resync_file_from_disk` in `crates/raven/src/backend.rs` is the shared primitive that converges one file's cross-file state (disk-content cache, workspace index, dependency graph) to its on-disk content. Two callers use it, so their semantics cannot drift:

- the `did_change_watched_files` CREATED/CHANGED async pass (external disk edits to closed files), and
- the `did_close` resync (`run_close_resync`), which discards graph topology derived from a discarded unsaved buffer.

Key properties (details in the function and commit-seam doc comments):

- All disk I/O, parsing, path-context discovery, and graph projection run off-lock. The resulting `PreparedClosedAnalysis` carries its `AnalysisBasis` to `WorldState::try_commit_analysis`, which performs the reopen veto and every authority mutation synchronously under one write lock.
- A missing file prepares a `ClosedRemove` mutation through the same seam. The watched-files path combines CREATED/CHANGED upserts and DELETED removals with one complete package projection in `PreparedWatchedBatchAnalysis`. Every watched mutation carries an exact `Valid(bytes)`, `Missing`, or `Invalid(bytes)` disk observation which is revalidated immediately before the CAS. Exact watched generations, the full package basis, every open-record token, and the frozen package-disk projection are checked with it, so a rewrite, delete/recreate race, or authority rejection changes neither closed-file nor package state.
- The watched package projection is rebuilt off-lock from DESCRIPTION, NAMESPACE, closed `R/` and `tests/testthat/` files, `data/`, `data-raw/`, `.Rprofile`, and testthat preamble closures. Frozen disk is authoritative for closed package-R paths; exact open buffers are replayed above it. `IfChanged` routing ownership is minted only when the effective package name/root changes.
- `old_meta` (for working-directory-change child invalidation) is caller-supplied: the close path must capture it **before** the document stores are wiped, because the unsaved buffer's metadata lives only there.
- Ordinary single-file commits reserve mutation-side fanout at the commit seam. `WatchedBatch` instead reports removal dependents from the pre-commit graph and upsert dependents from the post-commit graph as collect-only candidates; the outer watched finalizer merges them with package and system-file candidates, then deduplicates, activity-caps, force-marks, and assigns tickets exactly once.

If a watched overlay exceeds the traversal budget, Raven preserves arrival
order with per-file commits. Detached batches retain immediate commits;
invocation-owned batches use collect-only watched commits behind a counted,
generation-stamped diagnostics-coherence barrier. Diagnostic computation stays
lock-free, but final publication requires both a quiescent barrier and the same
generation captured with the diagnostic snapshot. A worker that overlaps the
multi-commit window therefore keeps its force marker and deduplicates a fresh
snapshot after quiescence instead of publishing an intermediate graph. The
barrier is admitted under the ordinary diagnostics publish lock, then carried
by the write-once invocation receipt across retries and deferred routing without
holding any lock across I/O. A package-only CAS runs first and installs its
analysis handoff durably before any sequential file commit can mint a
`system.file()` successor. Convergence is attempted only while that exact
package-routing owner remains current; a foreign or ABA-style successor
inherits the handoff and the stale batch stops without deriving or committing
against it. Invocation-owned items retry transient read failures while their
exact watched generations remain current. Source-batch parents are removed
from every reserved, transfer, and fallback handoff; their metadata refresh
commits while the barrier remains active, then the barrier releases before one
fresh parent diagnostic handoff. Supersession or shutdown cancels the shared
receipt; exact lifecycle-scoped marker retirement releases any already-reserved
diagnostic ownership without publishing the partial notification. Mixed
config/source notifications admit the same barrier before applying the config
mutation, so unrelated workers cannot publish the new config against the old
source graph.

Watched-file invalid UTF-8 is treated as potentially transient on the immediate pass: the resync preserves existing state and schedules a delayed retry. The retry carries the exact watched generation, so a newer CREATE/CHANGE/DELETE or close supersedes it. A still-invalid final read becomes a removal inside the same outer watched transaction; a recovered read becomes an upsert. Each arrival gets at most three complete inline attempts (initial plus two fresh recaptures). Exhaustion preclaims an additive `PackageSeedRetryLifecycle` owner: unrelated batches remain independently pending, while a same-URI successor makes the stale owner terminate without reseeding itself. Detached batches hand off and return under their existing acknowledgement contract. An invocation-owned immediate pass transfers its entire still-uncommitted notification to the delayed invalid-byte retry, preserving sibling and config coherence, and moves the same write-once completion receipt to that worker. The original handler remains pending until commit, supersession, or shutdown.

After a successful outer commit, package-wide exact open candidates, closed-file candidates, and any immediate routing/system-file transfer are merged through one activity-capped finalizer. If routing must continue asynchronously, that worker retains the entire untouched finalization bundle until the exact routing owner or a validated successor converges; package/closed candidates are never finalized early or recaptured by URI. CREATED/CHANGED handlers keep their asynchronous acknowledgement contract by spawning the batch, while DELETED-only handlers await commit and finalization before returning.

### Interactive request cancellation

Raven keeps `tower-lsp` at `.concurrency_level(1)` to preserve ordered text sync, which means tower-lsp's built-in `$/cancelRequest` notification can be delayed behind the in-flight request it is supposed to cancel. `start_lsp()` wraps the `LspService` in `RequestCancellationService`, which intercepts `$/cancelRequest` synchronously in `Service::call` and records cancellations in a request-id keyed registry before tower-lsp queues the notification.

Interactive handlers that can spend noticeable time in cross-file scope resolution should use a request-scoped `DiagCancelToken` from the registry and poll it through long loops and scope helpers. Qualified-member go-to-definition threads this token through `goto_definition_with_cancel` → `resolve_qualified_member_with_cancel` → `get_cross_file_scope_with_cache`, including per-candidate validation. New interactive multi-position resolvers should follow the same pattern: keep a request-local `ParentPrefixCache`, pass the token into every scope lookup, and check the token between candidate batches while still preserving a single `WorldState` snapshot.

### Semantic tokens

Raven advertises full-document semantic tokens for R documents and currently emits the standard LSP `function` token type for function-definition names and call heads. The token legend order is part of the LSP contract: keep `SemanticTokenType::FUNCTION` at index 0 unless all encoded token-type indexes are updated together.

### On-demand indexing

On-demand indexing pulls in files that are not currently open in the editor so their
symbols are available before diagnostics run. It happens **synchronously** in `did_open`
(gated by `raven.crossFile.onDemandIndexing.enabled`), covering, in order:
- direct sourced files
- the forward source chain (bounded by `maxForwardDepth`)
- backward-directive targets (bounded by `maxBackwardDepth`)

See `index_file_on_demand` / `index_forward_chain` / `index_backward_chain` in
`crates/raven/src/backend.rs`.

### Rmd/Quarto raw-content vs masked-analysis split (#343)

For `.Rmd` / `.Rmarkdown` / `.qmd` documents, everywhere we store or extract cross-file data we keep two views distinct (mirroring the `Document` type docs in `state.rs`):

- **Raw content** — `OpenDocumentRecord::document().contents`, `IndexEntry.contents`, and the `cross_file_file_cache` entry stay verbatim. `ContentProvider::get_content` returns raw, serving snippets and non-R-language text scans.
- **Masked analysis** — the `tree`, `metadata`, `artifacts`, and `loaded_packages` on those same records are derived from `chunks::mask_to_r` (chunk bodies only). Byte offsets in the stored `tree` index into `Document::analysis_text()`; pairing the tree with raw content mis-slices.

The state-aware closed-file chokepoints live in `state.rs`: `WorldState::analysis_text_for_uri(uri, content)` and `WorldState::extract_metadata_for_uri(uri, content)`. They consult `WorldState::editor_chunk_kind_overrides` first, then fall back to path classification, so extension-mismatched Rmd/Quarto files keep masking after close. The pure path-based helpers in `cross_file/mod.rs` — `analysis_text_for_path(path, content)` and `extract_metadata_for_path(path, content)` — remain for call sites that have raw content but no `WorldState`; their classifier masks `.Rmd` / `.Rmarkdown` / `.qmd` documents and borrows raw otherwise. Raw-content fallbacks therefore still contribute outgoing edges from chunks, never spurious prose-derived ones. `.Rmd` / `.Rmarkdown` / `.qmd` are intentionally excluded from the proactive workspace scan (outgoing-only); incoming relationships come from an open Rmd/Quarto document or a `.R` file's `# raven: sourced-by` backward directive.

## Package library internals

Raven prefers static parsing for package exports and uses R subprocesses selectively.

Key ideas:
- Parse `NAMESPACE` exports when available (fast)
- Detect `exportPattern()` and only then fall back to an R subprocess for accurate expansion
- If R is unavailable, fall back to `INDEX` parsing (best-effort)
- When merging `Depends` with meta-package `attached_packages`, keep a companion `HashSet` for dedupe; repeated `Vec::contains()` checks make recursive export expansion quadratic.

Library-path discovery keeps renv activation and diagnostic provenance in one
bounded R subprocess. For an explicit workspace containing
`renv/activate.R`, the probe records `.libPaths()` before and after the existing
activation, then emits a versioned hex-encoded frame. Rust accepts only the last
complete valid frame and verifies the activated project reported by renv
against the canonical workspace root. Source failure, malformed/truncated
output, missing paths, or a project mismatch fail closed to no renv evidence;
the ordinary active-path/fallback contract remains unchanged.

The pre-activation paths never enter `PackageLibrary.lib_paths`. They are
reduced immediately to conservative package evidence: a real package directory
with a readable regular `DESCRIPTION` whose `Package:` field agrees with its
valid directory name, plus its explicitly declared `NAMESPACE` exports. Names
present in the active paths, configured additional paths, or the base-priority
set are removed. The surviving `renv_outside_active_overlay` drives the
`package-outside-active-library` diagnostic and supplies positive-only export
evidence solely to the undefined-variable collectors after a true search-path
attachment. It must never feed ordinary export lookup, completion,
`package_exists()`, `system.file()`, watchers, enumeration, or freeze/build
commands. Library forks retain it, full rebuilds recompute it, and `clear_cache`
leaves it intact; adding an active library path prunes its names and exports
immediately. Outside-library filesystem changes intentionally require an
explicit package refresh. Diagnostic collection consults the overlay only when
the snapshot has exactly one workspace folder; multi-root routing activates the
first root globally, so attributing that evidence to any document in a
multi-root session would be ambiguous (especially for nested workspace roots).

`PackageLibrary` owns two in-memory side caches: `packages` for direct per-package metadata and `combined_entries` for aggregate availability/ownership snapshots. Both are atomic read-copy snapshots (`arc_swap::ArcSwap<HashMap<...>>`) with a small writer mutex per map to serialize copy-on-write publication. Synchronous cache readers load an immutable snapshot and do normal `HashMap` lookups, so an in-progress writer publication is never semantic absence and cannot produce transient diagnostics. Writers clone the current map, mutate the clone, and publish a new `Arc<HashMap<...>>`; keep those publication sections small and never hold a writer mutex across `.await`, R subprocess calls, disk/provider reads, or recursive package expansion. Multi-map operations such as invalidation publish the two maps sequentially rather than nesting writer gates. Completion readers should clone only the relevant `Arc<PackageInfo>` / `Arc<CombinedEntry>` handles from snapshots before doing string iteration/dedupe.

Logical cache operations also hold one shared library-operation lease from their
first load through every direct and aggregate publication. Library replacement,
explicit refresh, and libpath Changed/Dropped events take the exclusive side to
fork a coherent unpublished successor and again at the final routing commit.
Every started logical mutation advances an operation epoch; a candidate derived
from an earlier epoch cannot retire the current cache. Retiring a library makes
late cache writers return a typed retry signal, and authoritative warm callers
reacquire the current `WorldState.package_library` with a bounded retry. Never
wait on a library lease while holding `WorldState`: the only finalization order
is old-library exclusive lease, then `WorldState` write.

`WorldState::try_commit_library_routing` is the production commit seam for LSP
library replacements and content successors. Its typed basis binds the exact
old `Arc`, operation epoch, install/content/routing identities, readiness,
package configuration and inputs, workspace folders, and (for watcher events)
the never-reused watcher owner. Detached work derives the complete future
`system.file()` projection against a prospective routing stamp; the commit
revalidates external file observations and atomically publishes the new
library, identities, readiness/overlay, index/open metadata, graph/pins, watcher
owner, and one analysis-transfer handle. Help-cache clearing, watcher restart,
user-visible cleared counts, and diagnostic finalization are winner-only
effects after that commit.

Replacement drivers run two foreground attempts and one retained two-attempt
lifecycle, so their maximum is four complete attempts, not an open-ended loop.
Configuration reconciliation escrows every preexisting handle and exact
candidate while a replacement is deferred; repeated exhaustion retains the
same ledger with capped cancellation-aware backoff. Only typed `Superseded`
work may acquire a transfer from the current routing before finalizing that
ledger. `refreshPackages` returns a JSON-RPC error on either supersession or
exhaustion so the extension cannot report an uncommitted refresh as
“refreshed 0”. Libpath Changed/Dropped instead refork the exact watcher event:
two foreground attempts are followed by an awaited owned retry with capped
backoff until commit, shutdown cancellation, or watcher-owner supersession.
Scheduling unrelated configuration work is additive and cannot cancel that
event.

### Availability vs. ownership: unified aggregate entries (#407)

`combined_entries[pkg]` is the aggregate cache built by `get_all_exports` / `collect_exports_recursive`. Each `CombinedEntry` stores two projections from the same traversal:
- `exports`: *availability* — "is this symbol visible through `pkg`?" This flat aggregate suppresses undefined-variable diagnostics.
- `owners`: *ownership* — "which package owns the help topic / NSE policy?" For `library(tidyverse)`, `exports` contains `mutate` under the `tidyverse` key while `owners` records `mutate -> dplyr`.

The entry is published and invalidated as one immutable snapshot so a reader cannot observe aggregate availability without matching ownership. As recursion visits each contributor it records `owners.entry(symbol).or_insert(contributing_pkg)`. **First contributor wins**, and because the aggregate root is visited before its `depends`/`attached` members, the root owns its own exports while a member owns only symbols the root does not export itself. A symbol the root genuinely re-exports in its own namespace is attributed to the root — an accepted limitation, since names alone cannot tell a re-export from an original export.

Lookups:
- `find_package_owner_for_symbol(symbol, loaded_packages)` consults the unified entry through the snapshot cache contract above. If a present aggregate entry says the symbol is available but lacks ownership, it fails closed instead of falling back to aggregate attribution. If no aggregate entry is warmed, it falls back only to direct per-package exports. Used by hover, the help panel, signature help, the parameter resolver, and the NSE callee resolver (`table_verb_policy` step 3.5, consulted by `resolve_call_arg_policy` before its standard-eval fallback and by the issue #433 dots-forwarding inference in `NseAnalysis::build`).
- `get_owned_exports_for_completions(loaded_packages)` mirrors `get_exports_for_completions` but attributes each symbol to its owner for completion detail / resolve `data.package`.
- `is_symbol_from_loaded_packages` reads `exports` only — availability stays a pure yes/no check.

All R subprocess calls must:
- validate user-controlled inputs (package names, paths)
- use timeouts to avoid hangs
- route through `RSubprocess::execute_r_code{,_with_timeout}` rather than spawning `R` directly

Each `R` spawn is CPU-heavy — loading the base packages alone is 6–11s and pins a core. A global semaphore (`r_subprocess_semaphore` in `r_subprocess.rs`, sized to the available parallelism clamped to the range 2–8) caps how many R processes run at once, so a burst of callers (a 16-way test run, or many documents warming packages at once) cannot oversubscribe every core and starve the latency-sensitive 5s `formals()` queries past their timeout. For ordinary calls the permit is taken outside the per-call timeout, so queue-wait does not consume that child-execution budget. Package-routing calls additionally carry one outer wall deadline across semaphore queueing and child execution. `get_lib_paths()` may degrade ordinary R execution or semantic failures to fallback paths, but it must propagate a typed `OuterDeadlineExpired` unchanged; caching fallback paths after an expired routing authority would let stale work look successfully initialized. If you see ordinary R-gated tests time out only under heavy machine load, suspect spawn volume, not the child timeout value.

See `crates/raven/src/package_library.rs` and `crates/raven/src/r_subprocess.rs`.

## Package-export databases (CI / R-free resolution)

This subsystem lets Raven resolve package **export names** without an installed
package or a running R — the case that makes `raven check` usable in CI. It is an
**ordered fallback over three tiers**: Tier 1 (installed, authoritative — the
existing path above) → Tier 2 (a committed, repo-specific `.raven/packages.json`)
→ Tier 3 (a `names.db` sidecar). `names.db` is not bundled with the binary; installs download it with `raven packages update` for broad CRAN/Bioconductor coverage. R's base-priority packages are embedded in the binary (all 14; only the default-attached base-7 are always in scope). The user-facing contract lives in
[`docs/package-database.md`](package-database.md); this section is the internals.

**Critical invariant — names ≠ install status.** The databases feed *export
resolution* only (they suppress the undefined-variable storm when R is absent).
They **never** make a package count as installed. The missing-package diagnostic
is Tier-1-only and is suppressed by default in `raven check` (re-enabled with
`--report-uninstalled`). `package_exists()` is never wired to consult providers.
See `docs/diagnostics.md` and `crates/raven/src/handlers.rs`.

### The `package_db` module

A new top-level module (`crates/raven/src/package_db/`) — declared in
`crates/raven/src/lib.rs` only, while `main.rs` remains a thin shim — owns all
encoding/decoding. `package_library.rs` is not moved or rewritten; it only gains
the seam.

- **One serializable record:** `model::PackageRecord` is the on-disk projection
  of `PackageInfo` — `{ name, version, exports, depends, lazy_data }`, with all
  vectors kept **sorted and de-duplicated** so the Tier 2 JSON is deterministic
  and diff-friendly. `version` (the `DESCRIPTION` `Version`, `""` when unknown)
  rides in both encodings and drives the Tier 3 monotonic merge (see below);
  `PackageInfo` itself carries no version, so `from_info` defaults it to `""` and
  `into_info` drops it — the field is populated only by the capture paths
  (reference-R seed, `freeze`, r-universe ingest). `is_meta_package` /
  `attached_packages` are a pure function of the package name, so they are
  **re-derived on read** (via `PackageInfo::with_details`), never stored.
- **Two encodings, one reader each:**
  - **Tier 2 — `json_db.rs`** (`RepoDb`): deterministic, sorted, pretty JSON,
    committed as `.raven/packages.json`. Carries minimal provenance (Raven
    version, R version, generation epoch) and a `schema_version`
    (`REPO_DB_SCHEMA_VERSION`).
  - **Tier 3 — `binary_db.rs`** (`ShippedDb`): a compact, memory-mapped,
    lazily-decoded container. Layout: `MAGIC(8) | version(u32) | header_len(u32)
    | header(postcard) | payload`. The header (provenance + a `blake3` hex of the
    payload region + an index of `{name, offset, len}`) is decoded **once at
    open**; per-package `PackageRecord`s in the mmap'd payload are postcard-decoded
    **lazily** on lookup. Versioned by `FORMAT_VERSION`. Dependencies: `memmap2`,
    `postcard` (added in Milestone 1); `blake3` was already a dep.
- **Typed reader errors, explained not dropped (decision #9):** each reader
  returns `Absent` (normal, silent) vs `UnsupportedSchema { found, supported }` /
  `UnsupportedFormat` vs `Corrupt`. A present-but-unusable file (e.g. a
  `.raven/packages.json` written by a newer Raven) is **explained and the surface
  continues**: `raven check` prints a specific note (on stdout with the
  diagnostics for `text` output, on stderr for `json`/`sarif` — it rides the
  shared footer; see `footer_stream` in `cli/check.rs`), the language server
  raises `window/showMessage`, then resolution degrades to the next tier. A
  missing or unreadable database never hard-fails the LSP or the build.
- **Tier 3 locator order:** environment overrides still win, then the user-data
  sidecar installed by `raven packages update`, then an exe-relative sidecar
  (e.g. one placed next to the binary by hand). Installs normally have only the
  user-data candidate, since `names.db` is not bundled with the binary.

### The `PackageMetadataProvider` seam

Tier 1 stays a bespoke first step inside `get_package` — its subprocess/INDEX
logic is intertwined with `lib_paths` and the caches, and wrapping it would risk
regressing the authoritative path. The new tiers are pure pre-built reads, so the
seam is a small **synchronous** trait:

```rust
pub trait PackageMetadataProvider: Send + Sync {
    fn lookup(&self, name: &str) -> Option<PackageInfo>;
}
```

`PackageLibrary` gains an ordered `providers: Vec<Arc<dyn PackageMetadataProvider>>`
(Tier 2 repo DB at index 0, Tier 3 shipped DB next), empty by default. The **one**
hook is in `get_package`: when `find_package_directory` misses (Tier 1 has no
directory), it consults the providers in order; the first hit is cached like any
other `PackageInfo`. A package no provider knows is left **uncached**, so
`package_exists()` still reports it missing.

`prefetch_packages` is **unchanged** (decision #12): its Step 3 already funnels
every loaded package — and transitive `Depends` and meta-package
`attached_packages` — through `get_all_exports → collect_exports_recursive →
get_package`, which carries the hook. So `library(tidyverse)` and `Depends`
chains resolve in CI for free.

**Construction split (decision #6):** `freeze` (and the Tier 3 reference-R seed)
must capture a *provider-less* library so a "frozen Tier 1" file can't be
contaminated by Tier 2/3 guesses. Construction is split into
`build_library_inner` (no providers) feeding both
`build_package_library_tier1_only` (capture) and `build_package_library`
(runtime: inner + provider wiring). Existing 4-arg callers of
`build_package_library` are unchanged.

Runtime Tier 2/3 opening is globally coalesced by the exact repo/shipped-input
key. Each physical generation runs on one dedicated standard thread, independent
of any caller's Tokio runtime, under a process-wide cap of four. `Share`
callers—startup, didOpen/on-demand initialization, and ordinary builders—follow
the newest same-key watch generation and reuse its immutable `Arc` providers,
even while the physical cap is full. Explicit replacement authorities
(`refreshPackages`, configuration rebuilds, and libpath watcher rebuilds) use
`FreshAfterActive`: after FIFO capacity admission they replace the generation
that was active when they arrived, and every later `Share` caller follows the
successor. Old waiters retain the old terminal result; the two physical
same-key generations may temporarily consume two permits.

Registry installation and standard-thread spawn occur in one synchronous
critical section with no await. The thread-owned completion guard catches
unwind, publishes exactly one terminal watch state, compare-removes only its
exact `(key, generation)`, and drops the capacity permit last. Spawn failure
performs the same terminal publication/cleanup and returns a typed error. A
caller deadline or complete runtime shutdown can discard a receiver, but cannot
cancel the physical generation, release its permit early, or let an old
completion erase a newer generation.

Each Tier 2 or Tier 3 file is parsed from an already-open handle. The loader
captures that handle's stable identity (platform file ID plus size/timestamps),
then compares both the handle and the path after parsing. An atomic replacement
or in-place mutation retries once under the original load deadline; a second
identity mismatch returns typed `ConcurrentModification` instead of publishing
stale or mixed metadata. Tier 3 maps the same verified handle—it never stats one
path and mmaps a separately reopened file.

### Performance discipline (§12)

The LSP hot path (diagnostic collection) reads the in-memory package caches from
already-published snapshots and must not take a disk/page-fault stall. So:

- the Tier 3 index is **preloaded at open**; per-package payloads decode lazily
  from the mmap on first lookup;
- optionally, a one-shot `madvise(MADV_WILLNEED)` over the header/index region
  (~500 KB) right after `mmap` faults the offset table in before the first
  lookup — one syscall, eliminating even the first index page fault. A cheap
  nicety, not required for correctness, and a no-op on platforms without
  `madvise`;
- the `blake3` payload checksum is verified **at open on the dedicated provider
  thread** (decision #13) — ~10–20 ms once during library init, off the async
  runtime — catching truncation/corruption/tampering;
- provider results feed the existing per-package cache, so steady-state lookups
  stay in-memory and make no provider call.

### Tier 3 build pipeline

The shipped `names.db` is built **append-only** from an authoritative reference-R
capture of the build machine's **entire installed library**
(`enumerate_installed_packages` across all lib paths, via the Tier 1 path:
`NAMESPACE` + subprocess `exportPattern` expansion + `data/` datasets) **∪** CRAN
+ Bioc r-universe (`cran.r-universe.dev` and `bioc.r-universe.dev` per-package
`_exports`/`_dependencies`/`_datasets`). Each rebuild **seeds from the previous
database by default** (append-only; `--fresh` rebuilds from scratch) and **never
drops** a package. **The merge is version-driven and monotonic (decision #16):**
for each package it keeps the record with the **highest version across all three
sources** (prior DB, reference-R, r-universe), so a package is never moved to an
older export set — a newer CRAN/Bioc release beats an older reference-R install, a
newer local/dev build beats an older CRAN release, and a still-newer prior capture
is retained. Equal versions tiebreak reference-R → r-universe → prior. The
comparison reads a `version` field on `PackageRecord` (captured from `DESCRIPTION`
`Version` by the reference-R seed and `freeze`, or the r-universe JSON `Version`);
the field rides in both encodings, so Tier 2 `.raven/packages.json` shows version
changes in `git diff` too. The full
installed library is the point — hard-to-install / GitHub-only / internal
packages enter the floor only this way, and append-only keeps them. The
maintainer **bootstraps** the floor once from a richly-provisioned machine; teams
can build a private `names.db` from their own library for richer in-house
coverage.

- **Entry point:** `raven packages build-shipped-db` (maintainer/CI-only — the
  shipped binary itself is **network-free**; `scripts/build-names-db.sh` uses
  `curl` to download the r-universe metadata that the command then transforms).
  **Fetch is one bulk `/api/dbdump` per universe — 2 requests, not ~24k
  per-package (issue #371).** The per-package crawl ran 1h+ and needed a 5% skip
  tolerance; the BSON dump is the full-coverage single artifact (crates.io
  `db-dump` analogue) and runs in seconds. `cran.r-universe.dev`'s
  array/stream `/api/packages` endpoint is **not** usable — on that meta-universe
  it silently returns only the ~13% directly-hosted subset, so the dump is the
  only complete bulk source. `--runiverse-cran`/`--runiverse-bioc` accept either
  a `.bson` dump file (parsed by `parse_runiverse_dbdump`) or a directory of
  per-package JSON (the legacy fixture layout); `ingest_runiverse_path`
  dispatches on file-vs-dir. The script's `/api/ls`-count sanity gate aborts on a
  truncated dump. Code: `cli/packages.rs`, `package_db/{merge,runiverse}.rs`.
- **Embedded base packages (ADR 1):** all 14 of R's base-priority packages
  (`installed.packages(priority="base")`) are compiled into the binary as a
  `// @generated` per-package table in
  `embedded_base.rs` / `embedded_base_generated.rs`. Regenerate with
  `raven packages build-embedded-base --reference-lib <DIR>`. `initialize()`
  loads every package into the per-package cache (datasets → `lazy_data`) so
  `library(grid)` etc. resolve offline, but only the 7 default-attached packages
  (`get_fallback_base_packages()`: `base`, `methods`, `utils`, `grDevices`,
  `graphics`, `stats`, `datasets`) seed the flat always-in-scope `base_exports`
  set + `base_packages`. `names.db` excludes all base-priority packages
  post-merge (the `build-shipped-db` filter strips everything embedded, via
  `embedded_base_packages()`). A real R install
  still wins.
- **Delivery (decision #14):** one sidecar — `names.db` + checksums — lives on a
  **GitHub Release** (moving `names-db` tag) — durable across runs, unlike a
  per-run CI artifact. `raven packages update` is the explicit network boundary for
  source/Cargo installs and should be part of CI image setup/cache warmup, not
  normal LSP startup, completion, hover, package lookup, or `raven check`. A
  `workflow_dispatch` + scheduled job
  (`.github/workflows/build-names-db.yml`; weekly schedule `0 6 * * 1`) downloads
  the current asset (the seed), rebuilds via the shared
  `scripts/build-names-db.sh`, and re-uploads. `release-build.yml` does not
  bundle `names.db`: release archives ship only the `raven` binary, and the VSIX
  omits it too (VS Code users resolve their locally installed packages via Tier 1).
  A Git LFS seed is committed at
  `crates/raven/data/names-db-seed.db` for bootstrap/disaster-recovery only (not a
  build input). Located via the Tier 3 candidate locator — `RAVEN_NAMES_DB`
  override, then user data, then exe-relative. **Not** `include_bytes!` (avoids
  binary bloat), **not** committed to git (except tiny back-compat fixtures).
- The scheduled `build-names-db.yml` workflow stays on the default branch so GitHub Actions can run its cron, but its `actions/checkout` step is pinned to the protected `names-db-builder` branch. That branch is the production source line for the DB builder; merge only reviewed, release-compatible DB-builder changes into it.
- `release-build.yml` does **not** bundle `names.db`; release archives contain only the `raven` binary (plus `LICENSE`), and users obtain the sidecar with `raven packages update`. Before fanning out the platform builds, a `names-db-compat` job runs `raven packages validate-shipped-db` against the current `names-db` Release asset — a compatibility gate confirming the Raven being released can open and validate the database users will download — without bundling it.
- **Replacing the file (Windows mmap lock):** while `names.db` is mapped, Windows
  holds the file open (`CreateFileMapping` keeps a handle), so it cannot be
  overwritten in place. Writers (the bundle step, and any future in-process
  refresh) must use **atomic rename** — write `names.db.tmp`, then rename over the
  old file. On POSIX the rename succeeds even with the old inode still mapped
  (readers keep the old mapping until they re-open); on Windows an in-process
  refresh must **unmap → replace → remap**, tolerating a brief window with no
  mapping. This is a solved pattern (SQLite, clangd) but is called out so the
  implementation doesn't assume in-place overwrite works everywhere.
- **Growth bound:** append-only means the file grows monotonically, but slowly —
  at CRAN's ~2k-packages/year rate, ~1.7 MB/year, so a ~20–25 MB database stays
  under ~40 MB a decade out. Names-only storage keeps the bound comfortable; no
  pruning is planned.

### Dependency on #350 (package-dataset resolution)

`PackageRecord` captures `lazy_data`. [raven#350](https://github.com/jbearak/raven/issues/350)
(landed) wired dataset resolution into `collect_exports_recursive`, so a Tier 2/3
record's `lazy_data` is folded into the resolvable set automatically — *non-base*
package datasets resolve in CI with no extra work in this subsystem. (Base
datasets come via the embedded base table, above.)

### Testing approach

- **Runtime tempdir fixtures** for round-trip tests (write → open → lookup) of
  both encodings, plus bad-magic / version-skew / payload-tamper rejection.
- **Committed back-compat fixtures** (decision #15): tiny `names.db` +
  `.raven/packages.json` under `crates/raven/tests/fixtures/package_db/` guard
  format back-compat ("old fixtures still load").
- **No-R consumer integration test** (`tests/package_db_consumer.rs`): empty
  libpaths, `library(dplyr); mutate(...)` yields no undefined-variable;
  `library(dpylr)` behaves per the names-vs-install-status split.
- **Build-side differential** (deferred to the `build-names-db` job, where R + a
  sample corpus are available): diff `names.db` against `getNamespaceExports()`.
- **Generation round-trip** (on a machine with R): generate `.raven/packages.json`,
  read it back, assert parity with the live Tier 1 result.

## Other subsystems

Brief orientation for modules outside the cross-file and package-library subsystems.

### Stan parsing and generated builtin catalog

Stan documents have their own tree-sitter parse path, pinned by git revision in
`crates/raven/Cargo.toml`. `Document` retains one Stan tree per text revision;
diagnostics reuse that tree. The syntax collector walks only outer `ERROR`
nodes and standalone `MISSING` nodes in source order. The semantic collector in
`stan_diagnostics.rs` uses the same tree at the shared diagnostics-snapshot
dispatch seam. Stan documents deliberately contribute empty R
metadata, scope artifacts, and package sets, and R-only async filesystem
diagnostics never run for them. Keep those boundaries explicit when adding a
new document lifecycle or disk fallback path.

The native Stan/JAGS syntax collector builds exactly one language-specific
`foreign_syntax::DelimiterIndex` from the same geometry-preserving source used
for diagnostics. The scan is bounded to 4 MiB, 65,536 delimiter events, and
65,536 lexical spans, with cancellation checks at least every 4 KiB and after
scanning. Ambiguous spans have a separate source-ordered index so recovery-window
reliability checks remain logarithmic rather than rescanning all ordinary spans.
Stan strings, comments, and complete recognized `#include` lines are opaque;
unknown hash syntax and unfinished includes or other opaque constructs make
intersecting recovery ambiguous. JAGS `%...%` operators are atomic (including
`%#%`), comments are
trivia, and quote-bearing spans are deliberately ambiguous rather than treated
as invented JAGS strings. Reaching any bound makes later windows unreliable;
it must never produce a partial specific explanation.

Each outer `ERROR` owns its nested `ERROR` and `MISSING` recovery, so one grammar
recovery unit emits at most one finding. Delimiter classification uses the
highest bounded ancestor whose parent is `program`: smaller windows can miss a
closer borrowed by recovery, while a whole-document window lets independent
blocks interfere. For a parser `MISSING` closer, the collector requires the
parser-confirmed opener to be the lexical stack top, virtually inserts the
closer, and scans the remainder of that recovery unit. A structural mismatch is
specific only when a lexical scan finds exactly the same opener kind and closer
byte range as its sole mismatch and becomes fully balanced when that wrong
closer virtually replaces the expected one. A second mismatch, stray closer,
remaining opener, opaque span, or structural/lexical disagreement keeps the
owner generic. Pure lexical unclosed/stray fallback additionally requires the
same exact imbalance document-wide so a parser-borrowed closer cannot turn a
balanced document into a false explanation.

Delimiter classification runs before program-structure classification. The
latter accepts only direct, uniquely complete program recovery and validates
fragments with bounded in-process Tree-sitter reparsing: at most 256 KiB per
candidate and 4 MiB cumulatively per collection. Each attempted Stan block
wrapper spends the fragment bytes independently, so trying all canonical block
contexts cannot multiply the effective cumulative parser budget. Preliminary
child and sibling walks stream without proportional allocations, check
cancellation, and fail closed after 65,536 visited nodes. Stan tries the shared
canonical `stan::PROGRAM_BLOCKS` wrappers but never guesses a destination block.
JAGS checks complete model content, missing/duplicate/out-of-order sections, and
empty models with the in-tree clean-room parser and a legal synthetic sentinel;
it never invokes JAGS, R, a network service, or another runtime oracle.

Before delimiter classification, finite retention may skip work only when the
earliest possible recovery anchor (the top-level recovery window start) is later
than the last retained finding. After a non-specific delimiter result rules out
an earlier opener anchor, the collector may use the actual `ERROR` start to skip
program-structure and generic-range work that cannot displace the saturated
retained window. Equal starts remain eligible because range ends and messages
also participate in ordering. Traversal and cancellation checks continue after
retention saturates, preserving the rule that every finite result is the sorted,
exactly deduplicated prefix of unlimited output. After a Tree-sitter runtime or
Stan/JAGS grammar update, re-audit the exact recovery fingerprints in
`crates/tree-sitter-stan/test/corpus/error.txt` and
`crates/tree-sitter-jags/tests/quality_gates.rs` before changing policy.

Before Stan diagnostic parsing, `stan::mask_raven_directives` replaces
canonically recognized full-line Raven directives, plus the comment suffix of
a recognized trailing same-line ignore, with spaces while preserving every
byte offset, line ending, code prefix, and leading UTF-8 BOM. It reuses the
directive parser's recognition rules; Stan `#include` and unknown hash lines
remain visible to the grammar. Diagnostic masking and the local directive
metadata used by Stan semantic diagnostics must first use
`stan::directive_eligibility_view`, a geometry-preserving, full-document lexical
scan that blanks only hash bytes inside double-quoted strings, `//` comments,
and `/* ... */` comments. String and block-comment state persists across lines;
line-comment state resets at each LF. Within this diagnostic path, never run
canonical directive regexes on raw Stan lines, because marker-shaped prose
inside a multiline comment could otherwise become active diagnostic metadata
or suppression.
The only grammar compatibility check is a zero-width required file child on
`preproc_include`, which matches stanc's rejection of bare `#include`.

The fragment-aware semantic pass is eligible only when `program` has a direct
recognized block. It never discovers blocks below recovery nodes, never
descends into `ERROR`/`MISSING` for semantic roles, and propagates a
declaration-incomplete taint only through scopes that recovery may have made
uncertain. Top-level data, transformed-data, parameter, and
transformed-parameter taint flows to later blocks; model/generated-quantities
and lexical taint do not escape their scopes. Any `preproc_include` anywhere
fails the whole semantic pass closed because preprocessing is explicitly out of
scope, including includes after blocks or inside nested scopes. Shared
declaration type dimensions/constraints precede every name, while declarators
bind left-to-right and the current local is visible in its initializer, matching
the pinned stanc oracle. `profile_statement` owns a lexical frame even though
the grammar exposes its statement list directly. A direct recovery node in the
functions block contributes one or more exact, narrowly inferred likely
callable-name candidates; it never suppresses unrelated variable names.
Concrete/MISSING identifier validation prevents zero-width recovery
diagnostics. The fixed
`MAX_STAN_SEMANTIC_DIAGNOSTICS = 500` ordered retention set is
independent of the configurable syntax cap and remains bounded during
traversal; traversal does not stop at saturation so cancellation remains
responsive. Tests exercise traversal beyond saturation and 256 nested lexical
frames.

Stan snapshots parse local directive metadata solely for `# raven: var` /
`# raven: func` and suppression filtering. This does not populate R artifacts,
metadata indexes, source edges, packages, or cross-file scope. Combined Stan
syntax and semantic findings are stable-sorted by source range and exact-
deduplicated only after both independent collectors finish.

The fixture corpus under `crates/raven/tests/fixtures/stan/` separates compiler-
valid models, syntax-valid semantic/type failures, syntax-invalid models,
Raven-extension cases, include cases, and a deterministic generated matrix.
`editors/vscode/scripts/check-stan-diagnostics-fixtures.mjs` uses the exact-
pinned stanc3 package as a development-time oracle; it is not part of Raven's
runtime. CI runs the oracle after `npm ci`. Rust integration tests assert that
Raven stays silent for compiler-valid groups, distinguishes clear undeclared
variables in the semantic/type-invalid group, and reports bounded, stable,
UTF-16-valid syntax findings for every syntax-invalid case.

The separate external-model holdout is controlled by
`scripts/diagnostic-corpora.py` and documented in
`docs/diagnostic-corpora.md`. Its committed `check` is offline; full validation
materializes checksum-pinned official upstream models under
`target/diagnostic-corpora/`, runs the Stan oracle with `--check-external`, and
runs the ignored `external_model_diagnostics` Rust integration target. The
holdout is post-hoc and must not become a syntax authority. In particular, the
JAGS leg is fetch-only: it does not inspect JAGS implementation, parser sources,
or manual prose, and its official models were not manually inspected or used to
derive the clean-room grammar. Integration CI always keeps
the stable offline check present, expands to full validation on conservative
relevant paths and `main`, and release builds require full validation before any
platform build.

Stan and JAGS diagnostic enablement is enforced at the shared diagnostics-
snapshot dispatch seam before either collector runs. The per-language switches
do not affect document identity, parsing, target discovery, completion, hover,
or navigation; a disabled diagnostic branch returns a successful empty result so
live reconfiguration can clear stale publications. Both languages then dispatch
through the same native-syntax collector limit from the diagnostics snapshot.
`maxSyntaxDiagnosticsPerFile = 0` maps to unlimited;
every finite value retains at most that many unique candidates in an ordered
vector while the traversal continues to check cancellation. The observable
result removes exact duplicates before applying stable source-order truncation.
Do not move this limit onto the combined diagnostics vector: it intentionally excludes R
syntax, semantic, lint, package, and cross-file findings. The manual release
evidence harness reuses an already-parsed 100 KB tree to isolate collection and
reports serialized LSP payload bytes:

```sh
cargo test --release -p raven --lib benchmark_foreign_syntax_diagnostic_caps -- --ignored --nocapture
```

Stan completion and hover share `crates/raven/src/stan_builtins_generated.rs`, generated from compiler metadata exposed by the exact-pinned `stanc3` development dependency in `editors/vscode/package.json`. The npm package version (currently 2.39.1) and its exported compiler version (currently 2.39.0) are validated and recorded separately. Raven bundles the generated Rust data; it does not load `stanc3`, start a compiler subprocess, or use the network at runtime. Hover retains every callable name and at most 12 representative signatures per name, plus the full overload count. The generator also adds `_lupdf` / `_lupmf` aliases from their normalized compiler signatures, restores higher-order and embedded-Laplace callables omitted from the flat signature dump, and maintains the mapping from sampling-statement distribution names to canonical density or mass functions.

After changing the `stanc3` pin or the generator, regenerate and check the catalog from the repository root:

```sh
bun editors/vscode/scripts/generate-stan-builtins.mjs
bun editors/vscode/scripts/generate-stan-builtins.mjs --check
```

Keep the package pin, `EXPECTED_STANC_PACKAGE_VERSION`, `DOCS_VERSION`, generated version constants, `docs/hover.md`, and the stanc3 attribution in `README.md` and `NOTICE` aligned. CI runs the check after `npm ci`; `#[rustfmt::skip]` on the large generated arrays makes the generator output independent of the host Rust toolchain while the surrounding hand-written module remains subject to the normal formatting gate.

### Vendored Stan grammars

`crates/tree-sitter-stan/` vendors the MIT-licensed Stan and Stan-functions
Tree-sitter grammars used by Raven. Their generated C sources and JSON metadata
under `grammars/stan/src/` and `grammars/stanfunctions/src/` are committed. The
crate's `package-lock.json` and exact `tree-sitter-cli` 0.25.5 development pin
match the generator recorded by the committed parser and keep regeneration
independent of whichever Tree-sitter CLI happens to be installed globally.
Both grammars are generated with ABI 14.

After changing either `grammar.js`, regenerate and verify the committed output
from the vendored crate directory:

```sh
cd crates/tree-sitter-stan
npm ci
npm run generate
npm run check:generated
```

`check:generated` regenerates both parsers with the pinned local CLI and fails
when any committed generated source differs. Integration CI runs this check in
the formatting job, so grammar changes must include their regenerated output
and lockfile changes must remain intentional.

JAGS completion, hover, signature help, and built-in go-to-definition exclusion share `crates/raven/src/jags_builtins_generated.rs`. Its input is the checked-in factual manifest `editors/vscode/scripts/jags-builtins-4.3.2.tsv`, pinned to the official JAGS 4.3.2 SourceForge tarball and SHA-256 recorded in that file. The manifest contains independently verified names, aliases, arities, roles, and automatically loaded module ownership only. It deliberately contains no JAGS implementation code, source structure, manual prose, tables, examples, or comments. JAGS is GPL-2.0-only; Raven's catalog is a clean-room factual interoperability artifact, not copied JAGS expression. This is a maintainer provenance boundary, not legal advice.

Regenerate and validate the catalog from the repository root without network access or an installed JAGS runtime:

```sh
bun editors/vscode/scripts/generate-jags-builtins.mjs
bun editors/vscode/scripts/generate-jags-builtins.mjs --check
```

The generator validates the exact version, source URL/hash, default `basemod`/`bugs` boundary, schema, ASCII sort order, uniqueness, syntax classification, and module ownership before producing Rust. Keep the manifest header, generated constants, `docs/completion.md`, `docs/hover.md`, `README.md`, and `NOTICE` aligned. Runtime editor assistance combines the native parse tree with the recovery-oriented scanner in `crates/raven/src/jags.rs`; it never consults R scope/help/package machinery and never starts JAGS, R, or a network request.

### In-tree JAGS grammar

`crates/tree-sitter-jags/` is a clean-room Tree-sitter grammar targeting JAGS
4.3.2. It remains independently testable as a workspace crate and is also the
native parser for Raven JAGS documents. `Document` retains one JAGS tree per
text revision. Each edit in an LSP change batch is applied to the retained tree
with Tree-sitter's byte-and-point `InputEdit`; Raven then incrementally parses
the batch's final text once against that edited tree. Diagnostics reuse the
stored result without reparsing. Representative valid-to-invalid edits must
preserve the same diagnostic invariants as a fresh parse. Do not require
arbitrary invalid-to-invalid edits to produce an identical recovery topology:
Tree-sitter may legally retain a different error shape, so tests should assert
bounded, in-range, conservative findings rather than canonizing that shape.
This optimization is JAGS-only: R Markdown
masking and Stan directive masking make their raw-text edit coordinates
incompatible with the previously masked tree. JAGS documents contribute empty
R metadata, scope artifacts, and package sets, and every R-only async
enrichment, filesystem, formatting, and reference-search path rejects them
explicitly.

The syntax authority is the public JAGS command-line parse phase, captured by
the independently authored matrix and harness under `oracle/`. Do not inspect
or copy JAGS's GPL-2-only parser source or manual prose when maintaining the
grammar. Tree-sitter R and Stan are pinned MIT implementation references; the
production mapping and complete quality evidence live in
`PRODUCTION_MAPPING.md` and `QUALITY_GATES.md` inside the crate. The fetched
official-model holdout is post-hoc evidence only: it is not a third syntax
authority and must not be used to reverse-engineer or revise productions from
model contents.

Generated parser artifacts and an 806-outcome oracle manifest are checked in.
The manifest binds the deterministic matrix and quality corpus to their exact
sources, generator, harness, and pinned JAGS executable hashes. After editing
`grammar.js`, an oracle input, the generator, or the harness, run `npm ci`,
`npm run generate`, `npm run check:generated`, `npm run check:oracle`, `npm run
check:evidence`, and `npm test` from the crate directory, followed by `cargo test -p
tree-sitter-jags` from the repository root. The ignored live-oracle and
release-performance commands are documented in the crate README. CI checks
generation, oracle-manifest, and parser-evidence drift, the Tree-sitter corpus, Rust tests,
rustdoc, and release performance without installing JAGS. Raven's diagnostic
integration imports both committed manifests directly and asserts the complete
mapping through `.jags`, `.bugs`, and `.bug` independently: all 460 accepted programs are
silent and all 346 rejected programs produce at least one stable, in-bounds
syntax finding. The independently maintained
`crates/raven/tests/fixtures/jags/invalid_expectations.json` manifest pins exact
defect lines and bounded finding counts for all 75 curated syntax-invalid
cases, without copying their source text. The integration test requires exact
ID-set equality between that manifest and the corpus and exercises every case
through all three extensions. Separate malformed-input and release tests cover
mid-traversal cancellation, UTF-16 positions, exact-dedup-before-stable-order-
before-cap behavior for the shared configurable limit, edited-tree reuse, and
100 KB diagnostic latency.

Strict JAGS-dialect diagnostics claim `.jags`, `.bugs`, and `.bug`
(case-insensitively), plus untitled buffers explicitly identified by the client
as JAGS. All three suffixes share `FileType::Jags` and must follow identical
parser, diagnostic, lifecycle, CLI, and editor-assistance routes. They do not
claim general OpenBUGS, WinBUGS, MultiBUGS, or NIMBLE compatibility. Do not
broaden extension matching beyond these suffixes accidentally. Disk reads use the shared
BOM-aware decoder, while raw in-memory text remains byte-preserving so
diagnostic ranges stay correct.

### Quarto process lifecycle

The VS Code extension's Quarto implementation lives under
`editors/vscode/src/quarto/`. One activation-scoped lifecycle owns the
`QuartoCommandLifecycle`, `QuartoRuntime`, `QuartoRenderEngine`, preview panels,
and shared **Raven: Quarto** output channel in that extension-host window. The
`QuartoCommandLifecycle` owns each command's asynchronous continuation
(preview, render, and stop): `run` invokes the body synchronously — so Preview
can register its pending-start intent before yielding — then retains the
promise through preflight, binary discovery, subprocess work, and outcome
handling, rejecting new bodies once shutdown begins. The render engine
registers every child synchronously when it spawns and rejects new renders as
soon as lifecycle shutdown begins. Render stdout/stderr still stream in full,
but only a bounded tail is retained for result parsing and failure context.
Preview stop and both engines' deactivation paths detach Raven's exact stream
listeners before signaling. Each child has one shared ladder: deactivation
tightens an in-flight graceful stop instead of starting a competing sequence.
If a child does not confirm `close` after the bounded SIGKILL wait, teardown
logs a non-throwing abandonment warning and continues; cancelled or timed-out
renders also resolve their result at that bound. The detached child can no
longer write through the subsequently disposed output channel.

Preview startup failures capture their bounded raw tail and then claim that
same idempotent stop ladder themselves; the runtime's defensive stop cannot
double-signal. Runtime `Session` stop, shutdown, and recursive teardown convert
injected rejections into output-channel diagnostics plus settled ownership, so
a rejected promise cannot poison a predecessor chain or strand a generation.
One-shot render checks an already-cancelled token before spawn, preventing any
document code from running before the post-spawn cancellation hook exists.

Preview replacement is generation-based. `startOrRestart` claims and installs
the new generation synchronously before its first `await`; every asynchronous
continuation carries `{ key, generation }` and becomes a no-op unless that pair
still owns the live session. This applies to process exits, readiness probes,
external-URI mapping, panel updates, and panel-dispose stops. Keep that
discipline when adding another async phase so an old preview cannot overwrite
or terminate its replacement. If intentional Stop closes a child before
readiness, the resulting startup rejection is superseded rather than failed;
the stop owner alone emits `stopped` and removes the session after teardown.

Removing a predecessor from the live map does not remove it from lifecycle
ownership. The runtime registers it in a retirement set until its shared stop
promise settles. Each newly claimed session also records the exact predecessors
it displaced. Its memoized teardown stops itself and recursively tears down that
acyclic predecessor graph, so a replacement that supersedes an unspawned
intermediate session inherits the intermediate session's complete pending
drain. Retirement membership remains tied only to each session's own stop, not
the recursive teardown during replacement. Explicit and panel-dispose stops
keep the session current and add a retirement hold through teardown, so a
concurrent replacement can still discover and inherit the drain. Successful
pre-spawn drains clear the new session's predecessor array, and teardown clears
its captured predecessor references on settlement; do not retain historical
session/process wrappers from the live generation. Deactivation snapshots the
identity-deduplicated union of live and retiring sessions and starts immediate
shutdown on all of them; keep detached stopping children in that union when
changing restart sequencing.

Treat `vscode.env.asExternalUri()` results as installation-scoped: obtain a
fresh mapping for every iframe installation, and derive the iframe URL and CSP
from that same value. Never cache a mapped preview URI. Webview serializer
restore is deliberately inert: it restores a placeholder panel and never
spawns Quarto or executes workspace code. Deactivation disposes all preview
panels and clears their static registry; otherwise a disable/enable cycle would
retain callbacks and generation counters from the disposed runtime.

Extension deactivation awaits `stopAllQuartoForDeactivation()`. That one
thenable rejects new command bodies and starts preview and render shutdown
before disposing panels, so panel callbacks cannot start a second graceful
ladder. It waits for all three bounded aggregates — command lifecycle, preview
runtime, and render engine — and disposes the shared output channel last;
`context.subscriptions` disposal alone is not the awaited process-kill
mechanism. For debugging, run
**Raven: Show Quarto Output** and inspect the **Raven: Quarto** output channel.

### Judge-backed Tier 2 indentation (#611)

Tier 2 enters through `indentation::on_type_indentation` and uses the
judge-backed path in [`indentation/judge.rs`](../crates/raven/src/indentation/judge.rs).
The judge queries the same expectation engine as the indentation lint; see
`accepted_indents_for_lines`, `LineIndentExpectation`, and `IndentKind` in
[`linting/rules/indentation.rs`](../crates/raven/src/linting/rules/indentation.rs).
There is no secondary indentation engine: when repair-and-ask returns `None`,
the backend emits no edit and preserves the Tier 1/native indentation the
editor applied before sending `textDocument/onTypeFormatting`.

Maintain these boundaries:

- Repair-and-ask builds a virtual buffer with a sentinel identifier on an empty
  or closer-only cursor line, splicing exactly one region so the document's
  tree can be reparsed incrementally (`InputEdit` in `VirtualBuffer`). The
  probe line's own leading closers consume their openers from the scanned
  stack; pushed-down closers reappear once, below the sentinel and before the
  closers synthesized for the still-open outer delimiters, each on its own
  line so bracket changes retain the lint's closer-on-own-line classification.
  Its delimiter scan (`unclosed_delimiters_for_judge`) masks tree-covered
  strings, comments, and backquoted identifiers before synthesizing closers.
- The judge returns `None` for multiline string and
  backtick-identifier interiors; unanswerable positions; a tabs-mode editor
  (`insertSpaces: false`) or a real (non-string) tab inside the active
  context — the rows from the outermost unclosed opener or the reference
  line down to the probe — because the expectation engine measures character
  columns while the editor renders tab stops; a repaired buffer whose parse
  still holds an error intersecting the reference-to-probe row window; and a
  nearest checkable line above the probe (the reference line) that does not
  sit at a lint-accepted column — the expectation model accumulates from
  column 0, so only lint-conforming context can be answered without
  collapsing a user's offset indentation. Tabs and syntax errors on unrelated earlier
  statements do not disable the judge — the lint's own fold tolerates both.
- On-type queries use the per-line expectation fold
  (`accepted_indents_for_lines`), which collects and sorts the change list
  once and folds only the requested lines — never the whole-document maps the
  diagnostic pass builds.
- Whole-document `Either` linting also collects and sorts one style-neutral
  change list. It folds `Indented` first and returns immediately when that
  pass is clean (because `Either` is a superset); otherwise it folds `Aligned`
  from the same changes. Aligned-comment exemptions remain separate per pass,
  and accepted columns are merged only for lines rejected by every pass — do
  not restore a third eager whole-document union map.
- The judge always queries `accepted_indents_for_lines` with the internal
  `Either` union. `SelectionPrefs::from_config` is the direct image of the
  producer's independent `argumentStyle` and `infixContinuationStyle` axes;
  lint configuration never enters selection. Compatible same-named infix
  pairs and lint `Either` are clean by construction, while mismatched strict
  pairs remain observable configuration states.
- The lint-resolved indentation unit is deliberately shared while the
  indentation rule is active, including per-document `auto` resolution and
  overrides. If the rule is disabled the editor unit is used. Do not confuse
  this unit coupling with style coupling.
- Mismatch advice reuses the opposite strict producer fold, but it may credit
  the producer only when the nearest checkable reference line conforms to the
  union of the lint and producer passes — the same `Either` conformity gate
  the judge applies. An offset/nonconforming context makes the judge emit no
  edit and must keep the ordinary lint message. The lint does not construct
  the judge's repaired buffer, so any syntax-error tree conservatively keeps
  the ordinary message rather than risking false producer attribution.
- The per-document effective-lint-config cache is only for URIs in
  `WorldState::documents`. `raven check` workers pass one-document overlays
  while the shared document store remains empty; those one-shot resolutions
  must bypass the cache to avoid a contended, workspace-sized map with no hits.
- The VS Code client memoizes options last observed on visible editors so
  hidden tabs retain `detectIndentation`/status-bar values. A document becoming
  visible must resynchronize the payload, and `editor.tabSize` or
  `editor.insertSpaces` changes must invalidate the matching cached field;
  `editor.detectIndentation` changes invalidate both. Invalidation applies
  only to document scopes affected by that setting event;
  a folder-specific change must not erase another folder's detected values.
  Resource/language-scoped `editor.formatOnType` is synced too. The server
  compares the replacement map before resolving overrides or clearing its
  cache, making ordinary unchanged visibility syncs cheap. The
  `raven/documentIndentUnitsChanged` payload preserves the v0.14
  `units` contract exactly (`indentUnit` required in auto mode, empty in fixed
  mode); current producer gates travel in a separate optional `options` array
  that old serde consumers ignore wholesale. This prevents a custom old
  server from treating the lower-priority fixed client unit as a per-document
  override of a project `indentationUnit`.
- `IndentKind` tags preserve each column's provenance: neutral `Block`,
  argument `ArgumentBlock`/`OpenerAligned`, and infix
  `InfixBlock`/`ChainStart`. This lets an axis-level `off` return no answer
  only when that axis decides the probe, without disabling brace or assignment
  indentation. `TopLevel` merge contributions are never preference targets.
- `Aligned` infix changes use the shared expectation engine's owning-statement
  floor: `max(first_operand_column, statement_indent + unit)`. Program/brace
  children are statement owners, call arguments are statement-like owners,
  and transparent parenthesis/unary/assignment ancestors must not add a
  second floor.

#### Latency budget

On-type indentation must answer in p95 ≤ 16 ms at the bottom of a 10,000-line
R document and ≤ 50 ms on a 100,000-line stress document in a release build.
The Criterion benchmark
`on_type_indentation/enter_bottom_10000_lines` in
`crates/raven/benches/indentation.rs` measures the full
`on_type_indentation` path. Per-request recomputation of the line index,
masked intervals, and indentation-change collection is deliberate because
those inputs cannot be stale. Revision-keyed caching is declined until a
measured budget breach justifies the added invalidation state.

The same benchmark file measures whole-document default-`Either` linting on
both indented-clean and aligned-only 10,000-line inputs. Keep both cases: the
first guards the `Indented` fast path, while the second forces the shared
change list through both strict folds.

#### Declined findings registry

- **Escaped backticks in backquoted names:** R rejects them; commit
  `93a3ca03` pins this as a does-not-fire test. Reconsider if R's grammar or
  the supported parser begins accepting that spelling.
- **Scanner unification:** moot because the legacy scanners and fallback
  engine are deleted. Reconsider only if a second indentation producer is
  intentionally introduced.
- **Per-Enter O(document) prefix work:** accepted while it remains below the
  latency budget above. Reconsider after a reproducible benchmark breach.
- **Indent-unit-change republish filter resolves open documents twice:** this
  is a cold O(open documents) path that runs only on indent-unit-change
  notifications. It diffs both the effective indent unit and the resolved
  mismatch-advice producer policy (the notification also carries per-document
  `insertSpaces` and `formatOnType`, issue #614); the policy resolution is a
  few field reads on top of the config resolution. Reconsider if profiling shows those
  notifications on a hot path or workspaces with many open documents miss
  their budget.
- **Rmd language-config generator matches the assignment rule by regex text:**
  its exact-count assertion fails loudly. If that generator is touched, a
  shared regex constant is the preferred upgrade.
- **Deleting Tier 1 `onEnterRules`:** declined because issue #611 specifies
  the `<-`/`<<-` Tier 1 rule and #610 defines per-axis `off` as “Tier 2
  stands down; Tier 1 stands.” Reconsider only if those product contracts
  change.
- **Tabs-mode Tier 2 stand-down:** with `insertSpaces: false` or a real tab
  in the active context, the judge returns `None` and no Tier 2 edit is
  emitted (the deleted legacy fallback used to answer here). Deliberate: the
  expectation engine counts characters, not tab stops, and the indentation
  lint skips tab-led lines, so there is no lint-accepted set to select from.
  Reconsider only alongside a visual-column model for the lint itself
  (tracked as issue #618).
- **`rstudio-minus` alias does not map to the infix axis:** the permanent
  `raven.indentation.style` alias sets `argumentStyle` only, matching the
  last released behavior (v0.14, where chain alignment was unconditional and
  the style governed paren arguments only). The brief window on `main` where
  the judge projected `rstudio-minus` onto both axes was never released and
  is not a compatibility target. Reconsider only if a release had shipped it.
- **Strict lint `indented` flags formatter-`aligned` output:** deliberate
  combined-model semantics — a mismatched producer/lint pair is a valid user
  configuration state, not a bug; the deeper-only chain-start tolerance moved
  into `either` (the default), and the mismatch-advice suffix names the
  settings to reconcile only when the rejected column exactly matches the
  resolved producer pass. Reconsider only if the combined settings
  doctrine itself is revisited.
- **Mismatch advice cannot see the per-Enter tab-shaped-context bail:** the
  stable editor-policy half of this limitation was fixed in issue #614 — the
  additive options array now carries per-document `insertSpaces` and
  `formatOnType`, and `resolved_indentation_producer_policy` returns `None`
  when either disables the Enter producer. The judge's remaining bail — a
  real (non-string) tab inside the per-Enter active context — is
  position-dependent (the window runs from the
  outermost unclosed opener or reference row down to the probe) and has no
  stable per-document analog at diagnostics time, so it stays unmodeled: in
  a spaces-mode editor with a stray tab elsewhere in a flagged line's
  context, the advice can still attribute a column to a producer that would
  stand down on that exact Enter. Narrow by construction (the lint already
  skips tab-led lines themselves), and the settings-reconciliation guidance
  stays correct. Reconsider alongside a visual-column model (issue #618).

- **`package_state/`** — Derived state for R package mode. Owns workspace detection result, namespace model, per-file facts (exported symbols, roxygen tags), and the aggregate scope contribution. Fully derive-based: `derive_package_state()` recomputes the entire `PackageState` from inputs.

  **Local-dev overlay for `load_all()`.** `PackageLibrary` maintains a local-dev overlay that surfaces the workspace package's internal symbols (non-exported `R/` definitions, sysdata, `.onLoad` names, NAMESPACE imports). The overlay is keyed on a fixed sentinel package name (the `LOAD_ALL_SENTINEL` constant, `__raven_load_all__`), **not** on a name derived from `DESCRIPTION` — the workspace `DESCRIPTION` `Package:` name feeds only the user-facing display label (`load_all_owner_display`), never the overlay key. When Raven detects a `load_all()` call in a file, it resolves the internals through this overlay exactly as it would resolve an installed package via `library()`. The shared derived-package installer refreshes the overlay for both event-driven updates and complete prepared projections, so adding, editing, deleting, opening, or closing package inputs keeps it in sync without restarting the LSP. Every code path that *replaces* `package_library` (LSP libpath rebuild/init, and `raven check`'s `maybe_init_r`) must separately call `refresh_local_dev_overlay` afterward, since a fresh library starts with a `None` overlay. The overlay integrates with the three-tier export resolution model (Tier 1 always wins for an actually-installed package).

  **R/-change revalidation closure.** In `did_change_watched_files`, when a file under `R/` changes, Raven computes the set of URIs to republish diagnostics for using **graph reachability and artifact booleans only** — no scope resolution, no `WorldState` read lock held across the walk. The closure (`extend_affected_for_load_all_revalidation`) seeds from three routes: (1) the `load_all()` **carriers** — open docs whose artifacts have `calls_dev_load_all == true` and that sit under the package root; (2) for each carrier, its forward `source()` descendants and backward `source()` ancestors; and (3) when the workspace `.Rprofile` itself attaches the sentinel, every open doc the `.Rprofile` prelude applies to, plus that doc's graph neighborhood. This respects the locking discipline that diagnostic computation must not hold the `WorldState` read lock across cross-file scope resolution (see `CLAUDE.md`).
- **`package_namespace.rs`** — R package workspace detection (DESCRIPTION-based) and namespace model. Detects roxygen-managed packages by scanning for `#' @export` in `R/*.R` files.
- **`help/`** — R help text and HTML rendering. `text` submodule provides Rd2txt for hover/completion; `html` submodule shells out to R's `tools::Rd2HTML` for the webview help panel. Results are cached via `HtmlHelpCache`.
- **`libpath_watcher.rs`** — Filesystem watcher for `.libPaths()` directories using the `notify` crate. Watches recursively, debounces events, diffs directory snapshots, and emits `LibpathEvent::Changed` with added/removed/touched package deltas.
- **`qualified_resolve.rs`** — Resolves the RHS of `$` and `@` operators for go-to-definition. Collects member-assignment candidates from the defining file and cross-file scope contributors, validates via re-resolution, and tie-breaks by dependency-graph distance.
- **`parameter_resolver.rs`** — Resolves function parameter info for completion suggestions. Extracts parameters from AST for user-defined functions; queries R subprocess `formals()` for package functions. Uses an LRU cache for subprocess results.
- **`roxygen.rs`** — Parses roxygen2 (`#'`) and plain comment blocks above function definitions. Extracts title, description, and `@param` entries for hover and completion documentation.
- **`content_provider.rs`** — Unified file access abstraction. Respects the "open docs are authoritative" rule: open documents are read from the document store; closed files fall through to disk/cache.
- **`editors/vscode/src/knit/knit-output-panel.ts`** — Per-source-path webview-panel registry for the `Raven: Knit` output viewer. Keyed by `sourceUri.fsPath` (aligned with the in-flight gate in `knit-commands.ts`). Tracks a `previewColumn` static so new panels stack as tabs in a single column rather than scattering; `recomputePreviewColumn` runs on every `onDidChangeViewState` / `onDidDispose` and adopts a surviving panel's column when the recorded one empties. The companion `help/help-panel.ts` remains a singleton (one R-help context per session is the right shape for that domain). See `docs/superpowers/specs/2026-05-17-knit-panel-per-file-design.md`.
- **`editors/vscode/src/viewer-tab-icon.ts`** + **`version-gate.ts`** — Editor-tab icons for the four webview viewers, assigned via `WebviewPanel.iconPath`. Codicon ids: help → `question`, data → `table`, plot → `graph`, knit → `book`. `WebviewPanel.iconPath` only honors a `ThemeIcon` at runtime on VSCode ≥ 1.110 (microsoft/vscode#282608); Raven's engine and `@types/vscode` floors both stay at `^1.82.0`, so `viewerTabIcon` returns `undefined` on older hosts (leaving the default page icon) and `applyViewerTabIcon` is the single helper callers should use. `applyViewerTabIcon` centralizes the locally widened `WebviewPanel.iconPath` cast required because the 1.82 typings only accept image `Uri` icons even though VSCode ≥ 1.110 accepts a `ThemeIcon` at runtime. `version-gate.ts` stays free of any `vscode` import so the pure `meetsMinVersion` is bun-testable (`tests/bun/version-gate.test.ts`).

### `package_state/sysdata.rs` — sysdata symbol extraction

`crates/raven/src/package_state/sysdata.rs` extracts the symbols written to `R/sysdata.rda` (package-internal data) and the names bound by `.onLoad`/`.onAttach`. The strategy is AST-first:

1. **AST scan** — walks `data-raw/**/*.R` (recursively) looking for `usethis::use_data(..., internal = TRUE)` (also the `devtools::use_data` re-export and bare `use_data`) and `save(..., file = "...sysdata.rda")` calls. Also scans `R/*.R` files for `.onLoad`/`.onAttach` definitions and collects `assign("x", ..., envir = <ns>)` and `<ns>$x <- ...` bindings where the receiver is provably namespace-like (`topenv(environment())`, `asNamespace(...)`, `getNamespace(...)`, `parent.env(environment())`).

2. **R-subprocess fallback** — only when the AST scan finds nothing *and* `R/sysdata.rda` actually exists (e.g. sources that commit the binary `.rda` with no `data-raw/` generating script, like r-lib/cli). Loads the file via a one-shot R subprocess (`load()` into an empty env, then `ls()`), subject to the subprocess helper's 10-second timeout. The loader first freezes the exact bytes in a temporary file, and its cache key combines their digest with the R executable identity (configured path, canonical target, and stable metadata/file identity), so neither a workspace rewrite nor an in-place R replacement can silently reuse the wrong result.

   LSP startup installs the names as one detached `PreparedPackageProjection`, never as separate raw-input, derived-state, marker, and diagnostic writes. Its basis binds the exact seed/root, package raw+derived generations, package-library install/content generations, configured and active R identity, analysis config, exclusions, workspace folders, and open-document context. The R subprocess and derivation run off-lock; the frozen file observation and every authority are revalidated immediately before the atomic commit. Stale work gets three complete attempts before one lifecycle-owned delayed retry; a true seed/root successor retires the old owner. If that retry observes the same Missing/Invalid file state for one debounce, it drains without R, package, or diagnostic work instead of polling forever. A later exact `R/sysdata.rda` create/change watcher event schedules a fresh lifecycle. Commit returns the exact diagnostic candidates and optional `system.file()` routing owner, which converge through one capped finalization ledger. Startup merely schedules this work, preserving its previous non-blocking timing. Any R failure is fail-soft and cannot partially mutate package or diagnostic state.

   The trigger predicate is single-sourced in `backend::sysdata_r_fallback_needed` and the fallback runs in **both** the LSP startup path (`backend.rs`) and `raven check` (`cli/check.rs::maybe_load_sysdata_fallback`), so editor and CLI agree that a package's own `R/` code can reference its sysdata objects. The CLI keeps the synchronous compatibility adapter; the detached ownership protocol is LSP-specific. The names feed only package-mode scope — a user script doing `library(cli); emojis` still flags.

### `system.file()` source resolution

`system.file(package = "pkg", "path/to/file.R")` calls are used by packages to reference installed data files. Raven resolves these to concrete paths and adds them as forward-source edges so cross-file analysis works across package boundaries.

**Lifecycle:** Resolution is deferred until `lib_paths` are ready — the workspace scan may run before the package library is initialized. Startup, library replacement, every `LibpathEvent::Changed` (package install/removal under a watched libpath), and DESCRIPTION/NAMESPACE manifest changes run one detached convergence transaction (`backend.rs`). Each exact routing owner gets at most two complete attempts; a newer routing/library owner supersedes old work immediately. Package-seed callers additionally carry the exact installed-seed identity (raw-input, derived package-state, routing, and library generations plus a unique install ID) through capture and commit. Once a seed installs, cancellation cannot abandon its owner: contention returns a typed deferred result and schedules one coalesced exact-seed retry, while supersession returns a transfer owned by an explicitly validated current successor. Routing-changing open/close commits likewise retain their exact outer candidates until the required owner or a validated successor converges, then combine both ownership domains in one finalization; delayed retries never recapture candidates by URI.

Package-library replacement also has a synchronous pre-seal ledger in
`WorldState`'s replacement lifecycle. Cancellation may deposit transfer
handles, exact candidates, fallback URIs, post-seed ownership, and build notes
there without reacquiring the async state lock. A successful replacement CAS
adopts that ledger and collapses it with predecessor transfers into one fresh
`SystemFile` transfer; post-seed tails remain on the pending transfer until a
state-locked finalizer registers the existing retry owner. They are not carried
in caller-local commit effects. Routing-owned detached tasks are registered
through a shutdown-linearized task gate: shutdown retires diagnostic epochs,
fences new routing registrations, drains tails/retry owners without publishing,
closes the spawn gate, releases all locks, and only then waits for tracked
tasks. Do not introduce a raw `tokio::spawn` for work that owns or consumes
these ledgers. Watched-file retries and deferred routing finalizers inherit a
tracked parent token or register as a root at an untracked handler boundary;
their sleeps select the task owner's shutdown token, and spawn refusal retires
exact retry currency synchronously. A `packageMode = "disabled"` transition
likewise preserves the exact routing owner minted while removing the workspace
package and completes its tracked `system.file()` convergence before config
diagnostics are republished. That worker is exact-owner only: a re-enable or
newer routing writer supersedes it without letting the stale Disabled task
derive, commit, or finalize the successor. If its config caller is cancelled
after a commit, an acknowledged two-phase handoff keeps an exact fallback copy
with the surviving tracked root until the caller has finalized the transfer or
placed it in durable escrow; a dropped acknowledgement makes the root consume
that copy itself through the same one-shot transfer gate.

Libpath watching uses a state-owned lifecycle rather than a loose task and
handle. A prospective watcher journals events while buffering and becomes
active only in the same validated CAS that installs its exact owner; every
active install starts with a full rescan. Journal deliveries are RAII claims
acked only by the winning routing CAS, so cancellation remerges them with newer
events. Prearm owns close guards on both sides of the blocking setup boundary,
and setup pauses are journal-local in tests; cancellation or shutdown closes
the buffering journal immediately, while a late OS handle is torn down instead
of reactivating it. Consumers claim against the routing-task shutdown token.
`Changed` is a durable targeted invalidation, `Rescan` is a sticky full-scan
obligation used for bounded overflow, and `Dropped` is terminal for that
attachment. Retry delays select only their timer, journal closure, or shutdown,
so an event storm cannot collapse backoff. One primary attachment failure
may install one exact recovery watcher. If that recovery also fails, the
watcher-only CAS records an exact terminal-degradation reconcile obligation;
its tracked worker clears and fully warms the current library without a third
watcher attachment, retries in a bounded paced batch, then parks on routing
edges with a heartbeat. The shared `Notify` is paired with a monotonic wake
generation advanced before every notification, so another consumer cannot
drain the only edge while degraded repair is between its final attempt and
park; notifications are still consulted only at that boundary, preserving
retry pacing. Publishing a successor watcher owner and server shutdown are
both wake-generation edges, so an already parked degraded root exits promptly
instead of remaining tracked until the heartbeat. Routing-CAS attachment
failure does not create this
extra obligation because its clear/warm/derive transaction has already
committed.

**Retention:** `resolve_system_file_sources()` never clears `ForwardSource.system_file` and never drops unresolved entries — resolution state (`path`, `resolved_uri`) is recomputed from scratch on every pass. This is what makes the lifecycle events above recoverable without re-extracting metadata: an edge to a not-yet-installed package forms once the package appears, a stale edge to a removed package is cleared, and a workspace `Package:` rename re-targets self-package (branch 1) references. An unresolved entry is inert (empty `path`, no `resolved_uri`): the dependency graph and scope resolution produce no edge for it and the missing-file collectors skip it (`ForwardSource::exempt_from_missing_file_diagnostics` is the single predicate). Convergence covers both metadata stores — the workspace index (closed files) and the authoritative document store (open buffers) — and commits metadata, external installs/removals, graph edges, pins, and open metadata as one validated unit. The libpath consumer supplies a package filter, so unrelated sources are neither resolved nor disk-probed.

**Ownership and locking:** Capture clones the exact routing/library/package, index, graph, configuration, and open-record authorities under a short read lock. Resolution, external reads/parsing, and graph derivation run on owned data in a blocking worker with no `WorldState` guard. Every external candidate is opened and read once; its valid/missing/invalid identity and any parsed artifacts come from those same bytes. Existing dynamically indexed targets are refreshed in the same atomic transaction when a still-referenced URI has a different valid snapshot; open-authoritative, workspace-owned, pending, missing, invalid, and unchanged targets retain their current owner/content. A refreshed target replaces its full record, artifacts, outgoing graph node, and interface-change fanout even when the resolved URI itself is unchanged. Immediately before the central CAS, Raven rereads and compares every identity, then validates all central authorities and final open targets before mutating any tier. Index admission receives the pin set derived from the prepared graph, not the pre-transaction graph, so a tight LRU cap cannot evict newly reachable external targets before their graph edges commit. Raw file-cache contents are inputs only, never freshness authority. `raven check` retains one explicitly named CLI compatibility writer until CLI index installation is migrated as a separate ownership family; LSP callers and lifecycle tests do not use it.

`system.file()` graph derivation has one process-wide production execution
lane. Its semaphore and exact `(worker id, derivation key)` slot are one
ownership unit. After deadline-bounded admission, Raven installs the slot and
starts a dedicated standard thread. The waiter owns a shared
result/notification object, while the thread's completion guard retains the
exact lane, stores and notifies the result (including caught unwind),
compare-clears only its slot, then releases the sole permit last. A stale
completion token therefore cannot clear a successor, and immediate
back-to-back work never depends on asynchronous `JoinHandle::is_finished()`
bookkeeping. Thread-spawn failure synchronously clears the exact slot and
releases capacity.

Watched closed-file batches normally rederive every prepared peer against one whole-workspace overlay and commit atomically. If that overlay exceeds the configured traversal budget, the narrow fallback first commits any paired package projection under its exact watched CAS, then replays the prepared closed mutations in deterministic event order through the existing single-item immediate transaction. The fallback never acknowledges a watched generation by returning without a commit or durable retry.

**Re-resolution:** If a `system.file()` source edge resolves to a path that was previously reported as an error (`unresolved-source-path`), the diagnostic is retracted once resolution succeeds.

### Package-corpus workflow

`crates/raven/tests/package_corpus.rs` is a long-running `raven check` regression suite against real R package sources. It is `#[ignore]`d by default because it requires network access to fetch package tarballs.

**Two TOML ledgers** — committed at `crates/raven/tests/fixtures/package_corpus/`:
- `accepted_real_diagnostics.toml` — diagnostics that are real bugs in the package (true positives, accepted as known findings).
- `known_false_positives.toml` — diagnostics Raven emits incorrectly (tracked false positives, expected to shrink over time).

**Running the corpus:** Use `--ignored --nocapture` to activate:
```bash
cargo test -p raven --test package_corpus -- --ignored --nocapture
```

**Env-var filters** (combine freely):
- `RAVEN_CORPUS_GROUPS=base,recommended` — run only packages in the named group(s).
- `RAVEN_CORPUS_PACKAGES=dplyr,DT` — run only the named packages.
- `RAVEN_CORPUS_ALLOW_UNCLASSIFIED=1` — pass when a new diagnostic appears that hasn't been triaged yet; without it, unclassified findings fail the run.
- `RAVEN_CORPUS_KEEP_TEMP=1` — preserve fetched package sources for local inspection.

## Coding conventions (repo-level)

- Avoid `bail!`; prefer explicit `return Err(anyhow!(...))`.
- Prefer `log::trace!` in hot/verbose paths.
- Avoid blocking filesystem I/O on request handlers; use async or `spawn_blocking`.
- Be careful with UTF-16 vs byte offsets: convert before constructing tree-sitter `Point`s.

## Testing notes

- Unit tests live alongside modules under `#[cfg(test)]`.
- Property-based tests use `proptest`.
- Integration tests live in `crates/raven/tests/` (performance budgets, libpath watching, indentation). These require `--features test-support`.
- Bun tests (`tests/bun/`) cover TypeScript extension logic: plot viewer, data viewer, help viewer, send-to-R, config validation.
- VS Code extension tests (`editors/vscode/src/test/`) use `@vscode/test-cli` with Mocha.

### Async handler completion in backend tests

An LSP handler returning is not generally a completion boundary. Watched-file
finalization, `didClose` disk resync, and diagnostic workers can continue in
finite spawned tasks. Tests for those paths must arm the typed
`FinalHandoffCapture` immediately before the target handler, inspect its typed
handoff payload, release it, and await its causal completion receipt before
asserting durable state. The first finalizer to record owns the receipt.
Detached and recursive diagnostic spawn boundaries register a labeled token
before spawn; bounded-fanout workers instead inherit the causal context and
remain owned by the aggregate parent that joins them. The receipt completes
only after the root closes admission and all tokens and joined fanouts finish.
An abnormal token drop records cancellation/panic instead of silently looking
successful. A multi-root `didClose` owns one aggregate resync-phase root with
one pre-registered child per root; its typed payload still describes the first
final handoff, while completion spans every root. Quiescent didOpen/didChange
setup uses the same receipt around its exact analysis-revalidation tickets.
Diagnostic causal context crosses the bounded-fanout spawn boundary. When a
receipt-owned worker supersedes an older worker, it registers ownership of
that predecessor before cancellation; if the predecessor loses the
cancel-vs-gate-consume race, it transfers that ownership to the recursively
spawned `diagnostics-backstop-respawn` child before spawning. This accounting
is test-only and does not alter production scheduling or publication.

Watched-file retries and deferred routing share a last-owner claim lineage.
Ordinary finalization records a `Finalized` payload; if the entire lineage is
retired by shutdown, supersession, or a terminal veto before reaching that
handoff, its last owner records an empty `RetiredBeforeFinalHandoff` payload.
Tests expecting real diagnostic ownership must assert `Finalized`. Tests whose
subject is terminal retirement may assert the retired outcome, which closes
the receipt without treating it as a normal finalization. A panic during
last-owner retirement still records an abnormal causal-root exit.

Do not use `force_republish_count_for_test`, the pending-revalidation map, a
shared “latest” snapshot, or a sleep as a generic proxy for invocation
completion. Those are backend-wide bookkeeping surfaces: overlapping current
work may legitimately consume or leave one force marker. Tests whose explicit
subject is marker/prerequisite drainage may still poll that contract, but their
timeout is only a deadlock watchdog and must report the target lifecycle,
force/pending state, outstanding causal labels, and relevant permit state.

When a test intercepts consecutive attempts at the same one-shot pause seam,
it must arm the successor after observing arrival and before releasing the
predecessor. `DiagnosticsPublishPause::rearm_before_release` enforces that
ordering and rejects a handle from another registry or seam. Releasing one
pause and arming its successor separately leaves an uninstrumented scheduling
gap in which the worker can cross the seam.

Use `open_in_quiescent_workspace` when workspace scanning and package routing
are outside the test's subject. It disables those two startup authorities while
preserving on-demand cross-file indexing, and asserts that the fixture started
no host package initialization. Direct `LspService` fixtures that must preserve
their own workspace-index configuration still set `packages.enabled = false`
explicitly before their first `didOpen`; production-default package discovery
is not neutral test setup. Package-input tests use
`open_in_synthetic_package_workspace`: package mode remains enabled, but an
explicit empty ready build outcome is routed through the production package
commit before `didOpen` derives the document, so those tests cannot contend on
host R, provider loading, or library-path watchers. Tests
that assert a later operation's exact force-marker or pending-worker state must
also settle `didOpen` through its exact analysis-revalidation receipt before
arming the target operation. `open_in_settled_package_workspace` combines that
receipt with the synthetic package fixture. Actual R/provider behavior stays
in the package-library and R-subprocess suites; a backend ordering or rebuild
test may inject a one-shot synthetic build outcome only at the external
builder-result seam, leaving its coordinator, CAS, routing, and handoff
production-real.

Libpath prearm pause hooks are journal-scoped and return invocation-owned
arrival and worker-completion signals. A cancellation test must release and
await the exact blocking worker after the async owner retires. Do not infer
pause locality from a real watcher completing inside a fixed timeout:
FSEvents/thread startup is host-load-sensitive, while two journal-owned
arrival barriers prove that one invocation did not consume another's hook.

Close-resync concurrency is bounded by an `Arc<Semaphore>` owned by `Backend`.
Clones of one server share its four permits, while independent `LspService`
instances do not contend. Production keeps the `system.file()` derivation lane
process-wide because it bounds a physical CPU worker and its exact slot. Unit
test `WorldState`s instead own independent lanes: unrelated concurrent test
invocations cannot spend one another's fixed routing deadlines, while retries
within one invocation still serialize through one exact lane. Dedicated
physical-lifetime tests construct a lane explicitly and verify slot clearing
before permit release.

Two nearby fail-closed behaviors are deliberately not completion-harness
mechanisms. `did_close_second_ancillary_invalidation_stops_after_one_retry`
pins the two-attempt close CAS ceiling (the document remains open after a
second ancillary invalidation), while
`close_resync_pre_commit_rejects_changed_disk_snapshot` pins rejection of a
stale disk parse without an in-invocation retry. Changes to eventual retry
policy belong in a separate correctness design and must preserve reopen/disk
snapshot vetoes.

### Native lint parity audit

`crates/raven/src/linting/lintr_parity.rs` is the cross-rule compatibility
gate for every lintr equivalent Raven advertises. The matrix was derived from
r-lib/lintr's `tests/testthat/test-*_linter.R` files at commit
`603ab79e6db25d380c5ee96f35ffd6ba16d223aa` (3.3.0.9000, 2026-07-07) and
spot-checked against release 3.3.0.1. Its coverage assertion fails when a rule
is added to Raven without a matrix entry. Keep focused tests beside each rule
for exact messages/ranges, suppression behavior, parse-recovery cases, and
Raven-specific extensions; the matrix owns default clean-vs-violation behavior
across the full supported set.

The parity boundary is lintr's default, statically decidable behavior. It does
not imply support for every optional linter argument or analysis that depends
on an installed package namespace. Raven's `.lintr` loader warns and ignores
unsupported calls, and `docs/linting.md` documents the supported mapping.
During an audit, use a one-linter R oracle to disambiguate behavior before
translating a fixture, for example:

```r
lintr::lint(text = "x^2", linters = lintr::infix_spaces_linter())
```

Pin the upstream commit in the matrix comment when adopting changed upstream
behavior, and describe any intentional divergence in both the rule's module
docs and `docs/linting.md` rather than silently changing the fixture.

### AST inspection utility

For quickly validating tree-sitter node kinds, use the `inspect_ast()` helper (see `crates/raven/src/handlers.rs`). Run with `--nocapture` to print.

## Learnings

Canonical list: `AGENTS.md`. Prefer code comments over Learnings entries — see `AGENTS.md` header for the policy.
