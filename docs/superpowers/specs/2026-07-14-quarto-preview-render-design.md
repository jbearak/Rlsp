# Quarto Preview / Render / Stop — design (issue #624)

Status: revised after adversarial review. Implements CLI-backed `Raven: Quarto Preview`,
`Raven: Quarto Render`, and `Raven: Stop Quarto Preview` for `.qmd` files, delegating all
rendering to the public `quarto` CLI surface and displaying the preview HTTP URL in a Raven
webview iframe.

## Why this exists (and its relationship to the knit specs)

The 2026-05-16 knit spec rejected duplicating `quarto.quarto`'s live preview because that
implementation depends on Quarto's **private** editor-navigation/render-token protocol.
That constraint stands. This feature uses only the public CLI surface:

- `quarto preview <file> --no-browser --host 127.0.0.1`, parsing the served URL from output;
- `quarto render <file>`;
- process signals for stop.

No re-target protocol: switching the previewed file within one project restarts the process.
Motivation (from #624): users who disable `quarto.quarto` (dueling CodeLens rows; its
hard-coded REditorSupport R executor) currently lose integrated `.qmd` render/preview.

## Locked product decisions

1. **Preview keying: project-or-file.** Key = nearest ancestor directory containing a
   regular `_quarto.yml` / `_quarto.yaml` file (walk up from the file, unbounded by the
   VS Code workspace), else the file itself. This is a **Raven heuristic for keying and cwd only**
   — the CLI performs its own (richer) project discovery from the target path regardless;
   we make no claim of equivalence. Concurrent previews across different keys; re-preview
   within the same key stops the old process and starts a new one. cwd for spawn = project
   root, else the file's directory. Each registry entry also records the originating source
   file so Stop can find the entry even if `_quarto.yml` is created/removed while running
   (alias lookup by key OR source path). Previews are **window-owned**: registries live in
   this extension host; Stop affects only the current window (documented).
2. **Refresh: Quarto's built-in watcher.** No `--no-watch-inputs`, no Raven save listener.
   The served page self-reloads after the CLI re-renders.
3. **Gating: workspace trust + resolvable CLI only** for Preview/Render. Independent of the
   R-console activation gate and of `quarto.quarto`'s presence. Never call
   `vscode.extensions.getExtension('quarto.quarto')` anywhere in this feature. **Stop is
   exempt**: it operates on an existing runtime entry and requires no trust, no save, no
   frontmatter parse, and no CLI resolution.
4. **UI:** command palette + a NEW `raven.quarto` editor-title submenu (sibling of
   `raven.sendToR` / `raven.build`). Do NOT nest in `raven.sendToR`: its submenu-level
   `when` requires `raven.rConsoleEnabled`, and ~half its member commands rely on the
   wrapper for that gate. When-clauses: `resourceExtname =~ /\.qmd$/i` (case-insensitive
   flag — enumerated-case alternation misses mixed-case variants; cf. CLAUDE.md's
   mixed-case-extension learning). Preview/Render entries additionally gate on
   `isWorkspaceTrusted` (hide execution UI in Restricted Mode; handlers still re-check).
   Stop stays visible without trust (extname-gated); `raven.quarto.openOutputChannel`
   ("Show Quarto Output") is palette-visible ungated.
5. **`server: shiny` is rejected best-effort** from the document's own frontmatter with a
   clear message (requires the separate `quarto serve` lifecycle; deliberately deferred).
   Project-level (`_quarto.yml`, `_metadata.yml`, profiles) Shiny config is NOT detected —
   documented limitation; no `quarto inspect` call (too slow for a preflight).
6. **Unsaved docs:** trust check → open document → `isDirty` → `save()`; abort on refused
   save (knit-commands.ts pattern). Applies to Preview/Render only (see 3 for Stop).
7. **Grammar gap fix (issue requirement):** Raven contributes the `quarto` language but no
   grammar, so `.qmd` loses highlighting when `quarto.quarto` is disabled. Add a
   `contributes.grammars` entry mapping language `quarto` to the existing
   `text.html.markdown.rmarkdown` grammar (`./syntaxes/rmd.tmLanguage.json`), consistent
   with `syntaxes/SOURCE.md`; `tests/bun/grammar-contribution-paths.test.ts` gates paths.

## Empirical CLI facts (spiked on Quarto 1.9.38 + quarto.js source reading)

- `quarto --version` prints a **bare version** (`1.9.38`) — useless as an identity check.
  `quarto --help` contains the literal substring `Quarto CLI` → identity probe is
  `<bin> --help` + substring check (analog of `isPandocVersionOutput`).
- `quarto preview <file> --no-browser` writes **everything to stderr** (not stdout):
  pandoc echo, `Output created: spike.html`, `Watching files for changes`,
  `Browse at http://localhost:3715/`, `Listening on http://127.0.0.1:3715/`.
  → scan **combined stdout+stderr**.
- Output contains **ANSI escape sequences** → strip ANSI before matching. The captured raw
  spike output becomes a bun test fixture.
- **The browse URL is printed BEFORE the server binds** (quarto.js: browse message precedes
  `Deno.serve`). "URL parsed" ≠ "ready" → host-side readiness probe required (below).
- Under some remote configurations Quarto may print a proxy URL as `Browse at`. Therefore:
  **`Listening on <url>` is the trusted origin source; `Browse at <url>` contributes only
  the path** (it carries the document-specific path). If `Browse at` is absent, use the
  `Listening on` URL as-is; if `Listening on` is absent, accept a `Browse at` URL only if
  it passes the loopback validation below.
- `quarto render` also emits `Output created: <path>` — reuse the pure
  `parseRenderedOutputPath` (`knit/output-path.ts`), with the Quarto caller supplying
  `stripAnsi(stdout + '\n' + stderr)` (the helper parses one string; the knit caller
  concatenates the streams itself — same here). Relative results are resolved against the
  spawn cwd; if multiple matches, surface the last.
- Preview serves browser-previewable formats (HTML, PDF via PDF.js, text). Non-previewable
  formats (docx/pptx/epub) may still print a URL that 404s → probe/first-load policy below.

## Security model

The URL parser is a **security boundary**: rendered documents (workspace-controlled code)
write to the same stderr we parse, so a malicious document can print
`Browse at https://evil.example/`. Mitigations, in order:

1. Spawn with `--host 127.0.0.1` so the real server is loopback-bound.
2. `validatePreviewUrl(raw)`: accept only `http:` (never https/other schemes), hostname
   exactly `127.0.0.1`, `localhost`, or `[::1]`, a valid numeric port, and **no
   credentials** (`user:pass@`). Reject everything else; a rejected URL is a hard `failed`
   state (never silently fall back).
3. Trusted-origin correlation: origin from `Listening on`, path from `Browse at` (above).
4. Host-side readiness probe: bounded GET retry loop (e.g. 20 × 250ms) from the extension
   host against the **validated raw loopback URL** (host-reachable in all remote
   topologies since the CLI runs beside the extension host). Redirects are not followed
   cross-origin. Only after a response arrives do we map + frame. A final 404 on the
   browse path → `failed` state advising `Raven: Quarto Render` (non-previewable format),
   and the server is stopped.
5. `vscode.env.asExternalUri()` is called **fresh for each iframe installation** (VS Code
   forbids caching mapped URIs — tunnels can close); the shell is rebuilt with a CSP
   `frame-src` derived from that same mapping in the same step, so CSP and iframe src
   cannot drift (plot-viewer invariant, without the memoization).
6. "Open in Browser" passes the **raw loopback URL** to `vscode.env.openExternal`, which
   auto-forwards localhost on remotes (per the API contract; do not pre-map).

### Webview containment

- Outer CSP: `default-src 'none'; frame-src <exact mapped origin>; script-src 'nonce-…';
  style-src 'nonce-…'`. No `connect-src`: the outer document performs no fetches. The
  framed document's own subresources/WebSocket are governed by its own browsing context,
  not the outer CSP — Quarto's live-reload socket works untouched.
- Iframe: `sandbox="allow-scripts allow-same-origin allow-forms allow-downloads"`. The
  framed origin is genuinely cross-origin from `vscode-webview://`, so
  `allow-scripts allow-same-origin` is NOT the classic same-origin escape; the sandbox's
  job here is denying top-navigation, popups, modals, and other capabilities to
  workspace-controlled active content while htmlwidgets/Reveal.js/Observable still run.
- Outer shell script uses `iframe.contentWindow` only for sender-identity rejection in its
  message listener; it never reads the framed DOM. Host messages must have the empty or
  webview-host origin and pass exact-key protocol validation, while messages sourced from
  the framed page are ignored. The framed page cannot reach `acquireVsCodeApi()` or the
  outer DOM (same-origin policy).
- `localResourceRoots: []` — the panel serves nothing from disk.
- **Everything interpolated into shell HTML is escaped**: dynamic values enter via
  JSON-serialized `setState` seeds or `textContent` assignment in the shell script; the
  iframe URL is attribute-escaped; log/error text (CLI output, paths) is rendered with
  `textContent` only. Hostile fixtures (`<`, `"`, `</script>`, newlines) in bun tests.
- Failure surfacing is honest: a cross-origin iframe fires `load` even when blocked, so
  the shell arms an ~8s banner ("if the preview looks blank… Open in Browser") without
  claiming to detect the cause. No automatic `openExternal` ever.

## Architecture

New directory `editors/vscode/src/quarto/`, mirroring `knit/`'s pure/impure split.
Reused verbatim: `sendSignal` (`knit/process-signals.ts`; extend its doc comment to name
the quarto engines), `parseRenderedOutputPath` (`knit/output-path.ts`; add caller note),
`extractFrontmatter`/`parseFrontmatter` (`knit/yaml-frontmatter.ts`),
`csp_sources_for_external_base` (`plot/csp.ts`) applied AFTER loopback validation,
`applyViewerTabIcon`, `canonicalOpKey` (`knit/raven-knit-paths.ts`).

Deliberately NOT reused: `OperationRegistry` (one-shot phased ops + knit-specific
refcounting; the preview runtime needs generation-based lifecycle instead), and no generic
"cancellable-run" refactor of shipped knit/pandoc engines.

### The preview runtime (replaces the naive registry)

One activation-scoped Quarto lifecycle owns the output channel, resolver,
`QuartoRuntime`, `QuartoRenderEngine`, and preview panels. Created in
`registerQuarto(context)`; `extension.ts`'s `deactivate()` awaits the single
`stopAllQuartoForDeactivation()` thenable in its `stops` array
(`context.subscriptions` disposal is NOT awaited by VS Code and is not the kill path).
Shutdown sets both engines deactivating (new preview and render starts reject), snapshots
and starts terminating both kinds of child concurrently before panel disposal (immediate
SIGTERM + short bounded SIGKILL when no graceful ladder already owns the child), disposes
every preview panel and its module-persistent registry, waits with bounded paths, disposes
the shared output channel last, and clears module refs
so a later re-activation starts clean (serializer guard included). The preview snapshot is
the identity-deduplicated union of live map entries and the retirement registry, so every
preview child remains shutdown-visible even after losing generation ownership. Every
preview process is therefore sent the deactivation shutdown sequence, including a detached
child whose graceful restart stop has not settled yet. One per-child controller owns the
signal ladder: shutdown reuses and tightens an in-flight graceful sequence instead of
signaling twice. Preview stop and render cancellation/timeout/shutdown detach Raven's exact
stdout/stderr data listeners before signaling. A final bounded wait still prefers confirmed
`close`; if SIGKILL is not confirmed, teardown logs a non-throwing abandonment warning and
proceeds without allowing the surviving child to write to the output channel after it is
disposed. The same bound resolves cancelled and timed-out render results even if `close`
never arrives.

**Generation discipline (race safety).** The session map is `Map<key, Session>` where a
`Session` carries `generation: number`, monotonically increased **synchronously** at the
top of every `startOrRestart(key)` before any `await` (operation-controller.ts's
"claim before first await" contract). All async continuations — child `close`, URL parse,
readiness probe, `asExternalUri`, panel dispose — carry `{key, generation}` and no-op
unless they still match the live session. `stop()` is idempotent: one shared stop promise
per session; intentional stops are distinguished from unexpected exits by a flag set
before signaling. The new-key and source-alias lookups may identify different old
sessions after project-marker changes. Sequence for restart: bump generation + invalidate
every identity-distinct old key/alias owner + register those owners as retiring + replace
the session entry synchronously → record those exact owners as the new session's predecessor
set → await each predecessor's memoized recursive teardown → if this generation is still
current, spawn. Teardown concurrently stops a session and recursively tears down its acyclic
predecessor graph, so superseding an unspawned intermediate session inherits its complete
pending drain. Retirement remains separate: each session leaves the registry only when its
own shared stop promise settles, except an explicit/panel-dispose stop adds a teardown hold
while keeping the session current. That makes a concurrent replacement inherit the same
transitive drain and keeps shutdown discovery intact. Once inherited teardown succeeds,
the new session releases its predecessor array before spawning; teardown also releases its
captured predecessor references on settlement, bounding retained process history. Panel
close during a pending restart cancels the spawn via the generation check, while a stopped
session emits the terminal update only if it still owns the key after teardown. A startup
rejection caused by intentional Stop is superseded, not failed, so `stopSession` retains sole
ownership of the terminal `stopped` update and session removal.

### Files (all under `editors/vscode/src/quarto/` unless noted)

| File | Kind | Responsibility |
|---|---|---|
| `quarto-probe.ts` | pure | `isQuartoHelpOutput(stdout)`: contains `Quarto CLI` |
| `quarto-detect.ts` | impure | `QuartoResolver` mirroring `PandocResolver`, but **resource-scoped**: `resolve(docUri)` reads `getConfiguration('raven.quarto', docUri).get('path')`; cache keyed by the effective configured value (multi-root safe); probe via `--help` + substring with hard timeout, both pipes drained, output capped; PATH → platform fallbacks (macOS `/opt/homebrew/bin/quarto`, `/usr/local/bin/quarto`, RStudio-bundled; Windows `%LOCALAPPDATA%\Programs\Quarto\bin\quarto.exe`, `%PROGRAMFILES%\Quarto\bin\quarto.exe`; Linux `/usr/local/bin/quarto`, `/opt/quarto/bin/quarto`); invalidated on `raven.quarto.path` change |
| `quarto-project.ts` | pure (DI'd regular-marker-file predicate) | `findQuartoProjectRoot(startDir)`, `resolveQuartoContext(fileFsPath)` → `{ key, cwd, projectRoot }`; heuristic, single source of truth for keying+cwd |
| `quarto-project-fs.ts` | impure | Production `statSync(...).isFile()` adapter for project markers; missing paths, directories, and filesystem errors are not markers |
| `preview-url-parser.ts` | pure | `stripAnsi`; **streaming line decoder** with carry buffer for split lines (no whole-buffer rescans); emits `Listening on` / `Browse at` candidates; `validatePreviewUrl` (loopback-only, http-only, no credentials, numeric port); origin/path correlation; scanning stops after readiness; startup tail capped (~64 KiB) for error display |
| `quarto-frontmatter.ts` | pure | `isShinyServerDocument(fm)`: `server: shiny` or `server: { type: shiny }` only (best-effort; no `runtime:` borrowing) |
| `quarto-messages.ts` | pure | discriminated unions + exact-key-set validators (`webview-ready`, `open-in-browser`, `stop-preview`, `request-restart`, `load-timeout`, `report-error` / `state-update`) |
| `quarto-preview-html.ts` | pure | shell HTML builder; states `starting` / `serving` / `failed` / `exited-unexpectedly` / `stopped` / `restore-placeholder`; all interpolation escaped per the security model |
| `quarto-preview-engine.ts` | impure | `QuartoPreviewProcess`: spawn `quarto preview <file> --no-browser --host 127.0.0.1` (argv array, never shell; `detached` on POSIX; env inherited); stream both pipes to output channel + line decoder; ready = race(validated URL + probe OK, startup timeout constant, early exit → capped raw tail surfaced); `stop()` = detach stream listeners, then SIGINT→5s→SIGTERM→5s→SIGKILL via `sendSignal`, bounded final close confirmation with warning, idempotent shared promise; unexpected-exit callback |
| `quarto-process-teardown.ts` | impure | shared per-child stop/shutdown controller; one signal ladder, shutdown tightening, confirmed-close races, bounded SIGKILL abandonment, non-throwing warning |
| `quarto-preview-runtime.ts` | impure | `QuartoRuntime` + `Session` (generation discipline, live + retiring lifecycle ownership, shutdown, source-path aliases for Stop) |
| `quarto-preview-panel.ts` | impure | per-key panel registry; `enableScripts: true`, `localResourceRoots: []`, `retainContextWhenHidden: true`; **authoritative view-state lives host-side** and is re-posted on `webview-ready` and on `onDidChangeViewState` visible (hidden webviews drop messages); serializer adopt → **must reapply `webview.options` first** (VS Code doesn't persist them), then wire listeners, then placeholder HTML; NEVER auto-spawn on restore; `onDidDispose` → runtime stop (generation-checked); deactivation disposes all panels and clears the static registry |
| `quarto-render-engine.ts` | impure | activation-scoped one-shot `quarto render <file>` registry; `KnitEngineResult`-shaped result; CancellationToken + timeout use the shared bounded ladder and resolve even without `close`; bounded retained stdout/stderr tails; deactivation rejects new spawns, tightens any in-flight stop, detaches stream listeners, and terminates live process trees with bounded close confirmation |
| `quarto-commands.ts` | impure | preflight for Preview/Render only (uri → `.qmd` check (case-insensitive) → trust → save-if-dirty → shiny gate → `resolveQuartoContext`); Stop bypasses preflight (runtime lookup by key or source alias); render in-flight guard = small local `Map` keyed by canonical file path; **defined outcome branches** below |
| `index.ts` | impure | `registerQuarto(context)`: builds the activation lifecycle, registers commands + serializer (once per activation; guard reset on dispose so disable/enable works), config-invalidation listener; exports `stopAllQuartoForDeactivation()` coordinating panels, preview/render shutdown, and output disposal |

### Command outcome semantics

Render (mirrors knit's classify/renderOutcome discipline; no toasts inside `withProgress`):
`ok` + parsed path → info toast with `Open` / `Reveal` (remote-aware, reuse
`open-exported-file.ts` where the format matches, else reveal-in-explorer); `ok` without a
parsed path → honest "render succeeded (exit 0); see output channel"; `failed` → error
toast + `Show Output` (channel focused); `cancelled` → silent; `timedOut` → error toast
naming the timeout setting; `spawnError`/resolver failure → actionable "Quarto CLI not
found" with `Install…` (opens quarto.org/docs/get-started) and `Set Path…` (opens settings
`@id:raven.quarto.path`). Stop: `stopped` info; `already stopping` no-op; `no preview
running for this document` info. Preview failures render in-panel states (never toast-only)
plus the output channel.

### Preview data flow

command → preflight → `resolveQuartoContext` → `runtime.startOrRestart(key, …)`
(generation-checked) → panel `starting` → validated URL + readiness probe → fresh
`asExternalUri` → shell rebuilt (`serving`) with CSP `frame-src` = mapped origin, iframe
`src` = mapped URL → Quarto's watcher owns refresh. Startup failure/timeout/early
exit/invalid URL/probe failure → `failed` with capped raw output (escaped) + `Show Output`.
Post-ready unexpected exit → `exited-unexpectedly` banner, iframe left in place,
`Restart Preview`.

### package.json

- `contributes.commands`: `raven.quarto.preview` ("Quarto Preview"), `raven.quarto.render`
  ("Quarto Render"), `raven.quarto.stopPreview` ("Stop Quarto Preview"),
  `raven.quarto.openOutputChannel` ("Show Quarto Output") — category "Raven".
- `contributes.submenus`: `raven.quarto` ("Quarto"); `editor/title` entry gated
  `resourceExtname =~ /\.qmd$/i`; member entries: Preview/Render add
  `isWorkspaceTrusted`, Stop does not.
- `contributes.menus.commandPalette`: same gates; `openOutputChannel` ungated.
- `contributes.grammars`: `{ language: "quarto", scopeName:
  "text.html.markdown.rmarkdown", path: "./syntaxes/rmd.tmLanguage.json" }` (+ SOURCE.md
  note).
- Settings: `raven.quarto.path` (string, default `""`, `machine-overridable`, ADD to
  `capabilities.untrustedWorkspaces.restrictedConfigurations` and update that block's
  `description`), `raven.quarto.viewerColumn` (`active`/`beside`, default `beside`),
  `raven.quarto.render.timeoutMs` (integer, default 600000, `minimum: 1`, `maximum:
  2147483647`; the render engine defensively applies the same Node-timer cap).
- `activationEvents`: `onWebviewPanel:raven.quartoPreview`.
- Extension-only settings: do NOT touch `initializationOptions.ts` / `SETTINGS_MAPPING`.
  Add `quarto: 'quarto.md'` to `DOC_LINKS` in `generate-settings-reference.mjs`, then
  regenerate `docs/settings-reference.md` (drift-gated in CI).

### Docs

New `docs/quarto.md` (commands, settings, pipeline, security model, limitations: shiny
detection is per-document best-effort; window-owned previews; non-HTML/PDF formats; no
Codespaces guarantee). Update: `docs/knit.md` ("No `.qmd` rendering…" statements + the
"what Raven does not do" table), `docs/coexistence.md` (standalone Raven workflow +
trade-offs), `docs/limitations.md`, `README.md` features bullet, `CLAUDE.md` "What to
read", and `docs/development.md` (preview runtime lifecycle/generation discipline +
debugging via the output channel). Module doc comments per file stating their invariants.

### Tests

Bun (`tests/bun/`, no `vscode` imports):
- url parser: real ANSI fixture from the spike; chunk-split lines; both line forms;
  no-match; **hostile lines** (`Browse at https://evil.example/`, `localhost.evil`,
  `user:pass@127.0.0.1`, `ftp://127.0.0.1`, wrong-shape ports); origin/path correlation;
  capped tail behavior.
- probe (`isQuartoHelpOutput`), project-root walk (found at depth N; none; fs-root stop),
  shiny frontmatter (incl. `runtime: shiny` alone does NOT trigger), messages validators,
  resolver (configured/PATH/fallback/cache-by-value/invalidate; non-quarto binary
  rejected; two URIs with different folder-scoped paths), render-engine helpers,
  preview-html (CSP frame-src exactly the mapped origin; sandbox attribute exact;
  placeholder has no frame-src; hostile interpolation escaped).

Mocha (`editors/vscode/src/test/`, `awaitActive` after `showTextDocument`):
- commands: trust gate, save-before-run, shiny gate, `.qMd` mixed-case accepted.
- runtime: simultaneous Start/Start and Start/Stop, dispose-during-restart, stale close /
  stale mapping no-ops (fake process factory), idempotent stop, key alias after
  `_quarto.yml` appears/disappears.
- panel: create/reuse per key; dispose stops process; serializer restore reapplies
  `webview.options`, renders placeholder, never spawns; malformed persisted state;
  hidden-panel resync on visibility.
- menu gating: submenu `when` has `/\.qmd$/i`, no `raven.rConsoleEnabled`, no
  quarto.quarto reference; Preview/Render entries carry `isWorkspaceTrusted`.

Manual acceptance (issue #624): htmlwidget + Reveal.js interactivity in the frame; save →
auto-refresh; Remote SSH forwarding; no leaked processes across restart/replace/close/
shutdown (`ps` check); framed page cannot reach `acquireVsCodeApi`.

### Build order

1. Pure core + bun tests (parser+validator w/ fixtures, probe, project, frontmatter,
   messages, html).
2. Resolver + engines (fake-spawn tests) + runtime (generation discipline).
3. Panel + commands + `quarto/index.ts` + `extension.ts` wiring.
4. `package.json` contributions (incl. grammar entry) + settings-reference regen.
5. Docs.
