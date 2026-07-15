import * as crypto from 'crypto';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import {
    buildShellHtml,
    fontsCssDeclarations,
    isKnitOutputMessage,
    paletteCssDeclarations,
    type FontFamiliesUpdate,
    type VscodeThemePaletteUpdate,
} from './knit-output';
import {
    resolveFontFamilies,
    type ResolvedFonts,
} from './render-html';
import { inlineLocalImagesAsDataUrls } from './inline-images';
import { previewArtifactPaths } from './raven-knit-paths';
import { adoptPreviewArtifacts } from './preview-persistence';
import { getKnitGrammarRegistry } from './post-knit-renderer';
import {
    resolveActiveThemePalette,
    type ThemePaletteOutcome,
} from './vscode-theme-palette';
import { applyViewerTabIcon } from '../viewer-tab-icon';

/**
 * Webview panel that renders a single `.Rmd`'s rendered HTML output in
 * an `<iframe sandbox="allow-same-origin">` with Refresh / Open-in-
 * Browser / theme-toggle toolbar buttons.
 *
 * See `docs/superpowers/specs/2026-05-17-knit-panel-per-file-design.md`
 * and the prior `2026-05-17-knit-output-webview-design.md`.
 *
 * Per-source registry: one panel per `.Rmd`, keyed by `sourceUri.fsPath`
 * to match the in-flight gate in `knit-commands.ts` (which also keys by
 * fsPath so the same file under different relative URIs collapses).
 * New panels anchor to `previewColumn` so they stack as tabs in a
 * single "preview" column rather than scattering.
 *
 * Architecture (unchanged from the singleton implementation):
 *  - Outer Raven-controlled shell document owns the CSP (in `<head>`),
 *    the toolbar, and a nonce'd `<script>` that posts messages.
 *  - Inner `<iframe sandbox="allow-same-origin" srcdoc="…">` embeds the
 *    rendered HTML inline. `allow-same-origin` is required because a
 *    bare `sandbox=""` would give the iframe an opaque origin that
 *    bypasses VS Code's webview service worker (Electron falls back
 *    to DNS resolution, which fails with `ERR_NAME_NOT_RESOLVED`).
 *    Scripts, forms, and popups are still blocked.
 *  - `localResourceRoots` is confined to `path.dirname(outputPath)`,
 *    where rmarkdown's `_files/` figure directories sit.
 *
 * Singleton → per-source: subsequent knits of the *same* source reuse
 * the panel and swap iframe content. Knits of different sources open
 * separate panels. If a panel's `outputPath` rootDir changes (rare —
 * e.g. user edited `output_dir`), only that panel is disposed and
 * recreated in its current column (`localResourceRoots` is immutable
 * post-creation — same workaround `help-panel.ts` uses).
 */
export class KnitOutputPanel {
    private static instances = new Map<string, KnitOutputPanel>();
    private static previewColumn: vscode.ViewColumn | undefined;
    /**
     * Production-installed callback invoked on every panel disposal,
     * with the source .Rmd's absolute path. Wired in `knit/index.ts`
     * to OperationRegistry.requestPreviewDirDeletion, which refcount-
     * gates the actual rm -rf so in-flight exports keep their cached
     * `.md`/`figure/` until they finish.
     */
    private static onDidDisposeHandler: ((rmdAbsPath: string) => void) | null = null;
    static setOnDidDisposeHandler(handler: (rmdAbsPath: string) => void): void {
        KnitOutputPanel.onDidDisposeHandler = handler;
    }

    /**
     * Pin / unpin handlers wired by `knit/index.ts` to the
     * OperationRegistry. The panel uses these to hold the preview dir
     * alive across the Export ▾ QuickPick. Without the pin, the user
     * could open the QuickPick, dismiss the panel before choosing, and
     * then watch the still-pending export pipeline fail because the
     * disposal handler already removed the cached `.md`.
     */
    private static pinPreviewHandler: ((rmdAbsPath: string) => void) | null = null;
    private static unpinPreviewHandler: ((rmdAbsPath: string) => void) | null = null;
    static setPreviewPinHandlers(
        pin: (rmdAbsPath: string) => void,
        unpin: (rmdAbsPath: string) => void,
    ): void {
        KnitOutputPanel.pinPreviewHandler = pin;
        KnitOutputPanel.unpinPreviewHandler = unpin;
    }

    /**
     * Notify the panel for `rmdAbsPath` that an export op for that source
     * has started (`busy = true`) or ended (`busy = false`). Toggles the
     * webview Export button between its "open quickpick" idle state and
     * its "cancel in-flight export" busy state.
     *
     * No-op when no panel for the source is open: the editor-toolbar
     * Export entry creates a panel on success but during the export
     * itself there may not be one to update. Callers should fire the
     * notification anyway — when a panel does exist, having `busy=true`
     * arrive even slightly before the panel can also matter (the panel-
     * reuse path's `requestPalette` pattern mirrors this).
     */
    static notifyExportBusy(rmdAbsPath: string, busy: boolean): void {
        const instance = KnitOutputPanel.instances.get(rmdAbsPath);
        if (!instance) return;
        if (instance.disposed) return;
        void instance.panel.webview.postMessage({
            __ravenExportBusy: true,
            busy,
        }).then(undefined, () => {
            // postMessage can reject if the webview was disposed between
            // the disposed-flag check and the actual post; swallow.
        });
    }

    private panel: vscode.WebviewPanel;
    private rootDir: string;
    private sourceUri: vscode.Uri;
    private outputPath: string;
    private readonly output: vscode.OutputChannel;
    /**
     * The most recent concrete `ViewColumn` this panel was observed in.
     * `panel.viewColumn` is documented as "only set if the webview is
     * in one of the editor view columns" — it can transiently be
     * `undefined` (newly created panel, mid-drag) even when the panel
     * is in a real column. We snapshot the column whenever VS Code
     * reports a defined value, so reuse / reveal logic and the
     * preview-column recompute can fall back to "where the panel
     * last was" instead of `ViewColumn.Beside`.
     */
    private lastKnownColumn: vscode.ViewColumn | undefined;

    /**
     * Disposables tied to this panel's lifetime — VS Code theme +
     * configuration listeners that drive the live "Apply VS Code
     * theme" palette refresh. We dispose them in `onDidDispose` so
     * they never outlive the panel (otherwise a knit-and-close
     * cycle in a long session would accumulate closures over
     * disposed webviews and slowly leak the underlying postMessage
     * channel).
     */
    private readonly perPanelDisposables: vscode.Disposable[] = [];

    /**
     * Set to true the moment VS Code reports the panel has been
     * disposed. Theme/config listeners check this flag before
     * posting to the webview — a race where the user closes the
     * panel while a re-resolve is in flight would otherwise call
     * `webview.postMessage` on a disposed webview, which VS Code
     * tolerates but logs as a warning.
     */
    private disposed = false;

    /**
     * Tracks whether we've already logged a "could not resolve VS
     * Code theme" line to the output channel for THIS panel session.
     * One log line per panel is enough; further failures during the
     * same session are silent (the toggle still falls back cleanly
     * to the GitHub palette).
     */
    private themeResolveWarned = false;

    /**
     * The id of the theme our last successful resolution picked. Used
     * to deduplicate `[theme]` log lines: we re-log only when the
     * resolved theme actually changes (e.g. because the user swapped
     * themes or the webview just delivered the active editor.bg).
     * Without dedup the channel fills up on every body-class flip;
     * without ALWAYS-logging, the user sees only the FIRST resolution
     * — which is invariably the "no-bg-yet" guess that gets corrected
     * a few ms later.
     */
    private lastLoggedThemeId: string | undefined;

    /**
     * The webview's most recently reported `--vscode-editor-background`,
     * lowercased. Set by `handleMessage` on `themeContext` messages
     * from the webview script. The resolver uses this to disambiguate
     * between candidate themes whose kinds coincide — only the
     * actually-rendered editor background uniquely identifies which
     * theme VS Code is rendering.
     *
     * Undefined until the webview reports for the first time. While
     * undefined, the resolver falls back to "first candidate that
     * loads".
     */
    private latestEditorBackground: string | undefined;

    /**
     * Monotonically increasing generation tag for `pushVscodeThemePalette`
     * calls. Each call snapshots `this.pushGeneration` at entry; if a
     * newer push has started by the time it's ready to deliver, the
     * stale result is dropped. Prevents out-of-order delivery when
     * multiple resolves are in flight (theme listener + config listener
     * + webview themeContext reply can all fire within a few ms of one
     * another on a single user theme swap).
     */
    private pushGeneration = 0;

    /**
     * Most recent webview-reported `editor.background` value that
     * failed the hex-shape validator. Tracked so we log the warning
     * only on transitions, not on every body-class flip (the
     * MutationObserver in the webview re-reports on every theme-kind
     * toggle).
     */
    private lastRejectedEditorBackground: string | undefined;

    /**
     * Snapshot of the `candidateFailures` list we last logged. Lets us
     * re-log when failures change for the SAME resolved theme id, so a
     * user editing a candidate theme's JSON and breaking it still sees
     * the breakage in the output channel even though `outcome.themeId`
     * hasn't moved.
     */
    private lastLoggedCandidateFailuresKey: string | undefined;

    /**
     * The last known languageId of the source document. Cached so that
     * font resolution continues to work correctly after the user closes
     * the source buffer while the preview panel remains open. Updated
     * whenever we successfully locate the source document.
     */
    private cachedSourceLanguageId: string = 'rmd';

    /**
     * Open or update the panel for `args.sourceUri`. Returns
     * `{ ok: true }` on success, `{ ok: false, error }` if the rendered
     * file cannot be accessed (caller should fall back to
     * `revealFileInOS`).
     */
    static async showOrUpdate(
        context: vscode.ExtensionContext,
        args: {
            sourceUri: vscode.Uri;
            outputPath: string;
            output: vscode.OutputChannel;
            /**
             * The knit working directory (`root.dir`). Relative
             * `include_graphics` image paths resolve against it, so the
             * inliner can reach a workspace image the author referenced
             * relative to the project root rather than the preview dir.
             */
            knitRootDir?: string;
        },
    ): Promise<{ ok: true } | { ok: false; error: string }> {
        try {
            await fs.promises.access(args.outputPath, fs.constants.R_OK);
        } catch (err) {
            return { ok: false, error: err instanceof Error ? err.message : String(err) };
        }

        const key = args.sourceUri.fsPath;
        const rootDir = path.dirname(args.outputPath);
        const existing = KnitOutputPanel.instances.get(key);

        if (existing && existing.rootDir === rootDir) {
            existing.updateContent({
                sourceUri: args.sourceUri,
                outputPath: args.outputPath,
                knitRootDir: args.knitRootDir,
            });
            // Prefer the panel's *current* column; fall back to the
            // last-known column so re-knitting a hidden panel does
            // not relocate it to `Beside` (which moves the panel
            // away from its prior tab group).
            const revealCol =
                existing.panel.viewColumn
                ?? existing.lastKnownColumn
                ?? vscode.ViewColumn.Beside;
            existing.panel.reveal(revealCol, true);
            return { ok: true };
        }

        if (existing) {
            // localResourceRoots is immutable post-creation — dispose
            // and recreate in the same column. Scoped to this source;
            // other panels untouched. Same column fallback chain as
            // the reuse branch above.
            const column =
                existing.panel.viewColumn
                ?? existing.lastKnownColumn
                ?? vscode.ViewColumn.Beside;
            existing.panel.dispose();
            KnitOutputPanel.create(context, args, rootDir, column);
            return { ok: true };
        }

        // Anchor priority for a brand-new panel:
        //  1. recorded `previewColumn` (already resolved by at least
        //     one prior knit's `onDidChangeViewState`)
        //  2. any surviving instance's panel.viewColumn or
        //     lastKnownColumn (handles back-to-back knits before the
        //     first panel's column has resolved yet)
        //  3. `ViewColumn.Beside`
        const column =
            KnitOutputPanel.previewColumn
            ?? KnitOutputPanel.findExistingColumn()
            ?? vscode.ViewColumn.Beside;
        KnitOutputPanel.create(context, args, rootDir, column);
        return { ok: true };
    }

    private static findExistingColumn(): vscode.ViewColumn | undefined {
        for (const inst of KnitOutputPanel.instances.values()) {
            const col = inst.panel.viewColumn ?? inst.lastKnownColumn;
            if (col !== undefined) return col;
        }
        return undefined;
    }

    /** Visible only for tests. */
    static getInstancesForTesting(): ReadonlyMap<string, KnitOutputPanel> {
        return KnitOutputPanel.instances;
    }

    /** Visible only for tests. */
    static getPreviewColumnForTesting(): vscode.ViewColumn | undefined {
        return KnitOutputPanel.previewColumn;
    }

    /**
     * Visible only for tests — disposes every real `WebviewPanel` in
     * the registry, clears the Map, and resets `previewColumn`. Fakes
     * inserted via `setInstancesForTesting` do not expose `dispose`
     * and are skipped.
     */
    static disposeAllForTesting(): void {
        for (const inst of [...KnitOutputPanel.instances.values()]) {
            const maybePanel = inst.panel as unknown as { dispose?: () => void };
            if (typeof maybePanel?.dispose === 'function') {
                maybePanel.dispose();
            }
        }
        KnitOutputPanel.instances.clear();
        KnitOutputPanel.previewColumn = undefined;
    }

    /**
     * Visible only for tests — inject lightweight stand-ins into the
     * Map so `recomputePreviewColumn` can be exercised without real
     * `createWebviewPanel` calls. The recompute logic only reads
     * `inst.panel.viewColumn`, so duck-typing is sufficient.
     */
    static setInstancesForTesting(
        fakes: ReadonlyArray<{ key: string; viewColumn: vscode.ViewColumn | undefined }>,
    ): void {
        KnitOutputPanel.instances.clear();
        for (const f of fakes) {
            const stub = {
                panel: { viewColumn: f.viewColumn } as unknown as vscode.WebviewPanel,
            } as unknown as KnitOutputPanel;
            KnitOutputPanel.instances.set(f.key, stub);
        }
    }

    /** Visible only for tests. */
    static setPreviewColumnForTesting(col: vscode.ViewColumn | undefined): void {
        KnitOutputPanel.previewColumn = col;
    }

    /** Visible only for tests. */
    static recomputePreviewColumnForTesting(): void {
        KnitOutputPanel.recomputePreviewColumn();
    }

    /** Visible only for tests. */
    getPanelForTesting(): vscode.WebviewPanel {
        return this.panel;
    }

    /** Visible only for tests. */
    getRootDirForTesting(): string {
        return this.rootDir;
    }

    private static create(
        context: vscode.ExtensionContext,
        args: {
            sourceUri: vscode.Uri;
            outputPath: string;
            output: vscode.OutputChannel;
            knitRootDir?: string;
        },
        rootDir: string,
        column: vscode.ViewColumn,
    ): KnitOutputPanel {
        const panel = vscode.window.createWebviewPanel(
            'raven.knitOutput',
            'Knit Preview',
            { viewColumn: column, preserveFocus: true },
            {
                enableScripts: true,
                enableFindWidget: true,
                retainContextWhenHidden: true,
                localResourceRoots: [vscode.Uri.file(rootDir)],
            },
        );
        return KnitOutputPanel.wireAndRegister(context, panel, args, rootDir);
    }

    /**
     * Rebuild a panel that VS Code is restoring after a window
     * reload/restart, from the `{ sourceFsPath, outputPath, knitRootDir }`
     * record the shell's `setState` persisted (`knitRootDir` is the
     * relative-image resolution base; absent/empty in pre-field records,
     * which then fall back to the source directory). Called by the
     * `WebviewPanelSerializer` registered in `knit/index.ts` (which has
     * already confirmed `raven.knit.persistPreview` is enabled — when it
     * isn't, the serializer disposes the panel instead of calling here).
     *
     * The orphaned old-session artifact is adopted into the current
     * session's preview path (`adoptPreviewArtifacts`) so a later "Knit
     * again" is a normal in-place update. When nothing is left to adopt
     * the panel still rebuilds, showing the "knit again" placeholder
     * (`updateContent`'s read-failure branch) — the toolbar's Knit-again
     * button only needs the source URI.
     *
     * VS Code does NOT persist `webview.options`, so `enableScripts` and
     * `localResourceRoots` must be re-applied here. `enableFindWidget` /
     * `retainContextWhenHidden` are `WebviewPanelOptions` (fixed at
     * creation) and are restored by VS Code from the serialized panel —
     * they cannot be set post-construction and are intentionally not
     * touched here.
     */
    static async restore(
        context: vscode.ExtensionContext,
        panel: vscode.WebviewPanel,
        state: unknown,
        output: vscode.OutputChannel,
    ): Promise<void> {
        const s = (state ?? {}) as {
            sourceFsPath?: unknown;
            outputPath?: unknown;
            knitRootDir?: unknown;
        };
        const sourceFsPath = typeof s.sourceFsPath === 'string' ? s.sourceFsPath : '';
        const persistedOutputPath = typeof s.outputPath === 'string' ? s.outputPath : '';
        // Recover the persisted resolution base; empty string means the
        // record predates this field, so `updateContent` falls back to
        // the source directory.
        const persistedKnitRootDir = typeof s.knitRootDir === 'string' && s.knitRootDir.length > 0
            ? s.knitRootDir
            : undefined;
        if (sourceFsPath.length === 0) {
            // No source recorded — nothing actionable to restore.
            panel.dispose();
            return;
        }

        const sourceUri = vscode.Uri.file(sourceFsPath);

        // A live panel for this source may already exist when the
        // serializer fires (a knit completed during activation, or
        // "Developer: Reload Webviews"). Keep the existing one as
        // authoritative and drop the duplicate VS Code just handed us —
        // registering the new one would orphan the old panel and its
        // dispose handler. (`create` never hits this because `showOrUpdate`
        // resolves any existing instance before calling it.)
        const existing = KnitOutputPanel.instances.get(sourceUri.fsPath);
        if (existing && !existing.disposed) {
            const col = existing.panel.viewColumn ?? existing.lastKnownColumn;
            existing.panel.reveal(col, true);
            panel.dispose();
            return;
        }

        let current: { previewDir: string; htmlPath: string };
        try {
            const paths = previewArtifactPaths(sourceFsPath);
            current = { previewDir: paths.previewDir, htmlPath: paths.htmlPath };
        } catch {
            // Session state not initialized (shouldn't happen — registerKnit
            // runs before the serializer can fire). Bail gracefully.
            panel.dispose();
            return;
        }

        const adopt = adoptPreviewArtifacts(current, persistedOutputPath);
        const htmlPath = adopt.htmlPath;
        const rootDir = path.dirname(htmlPath);
        // Re-apply the WebviewOptions VS Code dropped on serialization.
        panel.webview.options = {
            enableScripts: true,
            localResourceRoots: [vscode.Uri.file(rootDir)],
        };
        if (!adopt.available) {
            output.appendLine(
                `[restore] ${sourceFsPath}: no rendered artifact to restore ` +
                `(${adopt.reason}); showing the knit-again placeholder.`,
            );
        }
        KnitOutputPanel.wireAndRegister(
            context,
            panel,
            { sourceUri, outputPath: htmlPath, output, knitRootDir: persistedKnitRootDir },
            rootDir,
        );
    }

    /**
     * Shared post-creation wiring for both `create` (fresh panel) and
     * `restore` (serializer-rebuilt panel): construct the instance,
     * register it in the per-source map, anchor the preview column,
     * attach the view-state / dispose / theme / config listeners, and
     * render the initial content. Keeping this in one place means the two
     * entry points cannot drift in their lifecycle handling.
     */
    private static wireAndRegister(
        context: vscode.ExtensionContext,
        panel: vscode.WebviewPanel,
        args: {
            sourceUri: vscode.Uri;
            outputPath: string;
            output: vscode.OutputChannel;
            knitRootDir?: string;
        },
        rootDir: string,
    ): KnitOutputPanel {
        const key = args.sourceUri.fsPath;
        // Tab icon is set here (the shared path) so `create` and `restore`
        // can't drift on it.
        applyViewerTabIcon(panel, 'book');
        const instance = new KnitOutputPanel(context, panel, rootDir, args);
        KnitOutputPanel.instances.set(key, instance);

        // Anchor the preview column on the first panel that has a
        // resolved column. Subsequent new panels open in this column
        // so they stack as tabs rather than scattering to Beside.
        const resolved = panel.viewColumn;
        if (resolved !== undefined) {
            instance.lastKnownColumn = resolved;
            if (KnitOutputPanel.previewColumn === undefined) {
                KnitOutputPanel.previewColumn = resolved;
            }
        }

        panel.onDidChangeViewState(() => {
            const col = panel.viewColumn;
            if (col !== undefined) instance.lastKnownColumn = col;
            KnitOutputPanel.recomputePreviewColumn();
        });
        panel.onDidDispose(() => {
            // Guard against a stale dispose listener for an instance
            // that has since been replaced under the same key (the
            // rootDir-mismatch branch disposes the old panel and
            // inserts a new one). VS Code's dispose() is synchronous
            // today, but the guard makes the invariant explicit and
            // survives any future async change.
            if (KnitOutputPanel.instances.get(key) === instance) {
                KnitOutputPanel.instances.delete(key);
            }
            KnitOutputPanel.recomputePreviewColumn();
            // Run per-panel disposables so theme / config listeners
            // stop holding references to this panel. Set `disposed`
            // BEFORE running them so any listener mid-flight sees
            // the flag and short-circuits its postMessage call.
            instance.disposed = true;
            for (const d of instance.perPanelDisposables) {
                try { d.dispose(); } catch { /* swallow */ }
            }
            instance.perPanelDisposables.length = 0;
            // Signal the registered cleanup handler that the per-source
            // preview temp dir can be removed (subject to refcounting —
            // in-flight exports may still need it). Production wires
            // this in knit/index.ts to OperationRegistry.requestPreviewDirDeletion.
            const handler = KnitOutputPanel.onDidDisposeHandler;
            if (handler) {
                try { handler(instance.sourceUri.fsPath); } catch { /* swallow */ }
            }
        });

        // Listen for VS Code theme changes and the editor.* settings
        // that drive token coloring. Each event re-resolves the
        // palette and pushes a replacement CSS string into the
        // webview, so the user sees their newly selected theme's
        // colors without re-knitting. Bound to the panel's lifetime
        // via `perPanelDisposables`.
        //
        // Cross-kind theme swaps (e.g. Solarized Dark ↔ Dark 2026,
        // both kind=Dark) are the tricky case. The outer-shell body
        // class doesn't change because the kind is the same, so the
        // webview's MutationObserver doesn't fire — meaning the
        // webview NEVER re-reports its bg, and our cached
        // `latestEditorBackground` would stay on the OLD theme's
        // value. The resolver's disambiguation would then pick the
        // wrong candidate (matching the stale bg instead of the new
        // one).
        //
        // Fix: invalidate the cached bg on every theme change AND
        // poke the webview to re-report. The webview's CSS variables
        // have already updated to the new theme by the time this
        // listener fires (VS Code updates webview CSS vars
        // synchronously on theme change), so the next
        // `reportThemeContext` call returns the new bg.
        // Both listeners route through one helper. Guarded on
        // `instance.disposed` so a queued event firing during the
        // synchronous dispose sequence cannot try to postMessage on
        // a torn-down webview. (VS Code tolerates that today, but
        // the warning ends up in the developer console for no
        // benefit — and the guard makes the invariant explicit.)
        const onThemeChange = (): void => {
            if (instance.disposed) return;
            instance.latestEditorBackground = undefined;
            // postMessage can reject if the webview is disposed
            // between the guard above and the call. Swallow rather
            // than leaking an unhandled-rejection — pushVscodeThemePalette
            // already follows up with its own postMessage on a separate
            // generation, so a lost re-report request is harmless.
            Promise.resolve(
                instance.panel.webview.postMessage({ __ravenRequestThemeContext: true }),
            ).catch(() => {
                /* webview gone — pushVscodeThemePalette logs on its own postMessage path */
            });
            void instance.pushVscodeThemePalette();
        };
        const onTheme = vscode.window.onDidChangeActiveColorTheme(onThemeChange);
        const onConfig = vscode.workspace.onDidChangeConfiguration((e) => {
            // `workbench.colorCustomizations` is in the filter because
            // a user override like
            // `{ "editor.background": "#101830" }` changes the
            // webview's reported `--vscode-editor-background`, and the
            // resolver's disambiguation-by-bg path needs to re-fire
            // against the new value. Without it the panel can sit on
            // a stale bg and pick the wrong same-kind candidate.
            if (
                e.affectsConfiguration('workbench.colorTheme')
                || e.affectsConfiguration('workbench.preferredLightColorTheme')
                || e.affectsConfiguration('workbench.preferredDarkColorTheme')
                || e.affectsConfiguration('window.autoDetectColorScheme')
                || e.affectsConfiguration('workbench.colorCustomizations')
                || e.affectsConfiguration('editor.tokenColorCustomizations')
                || e.affectsConfiguration('editor.semanticTokenColorCustomizations')
            ) {
                onThemeChange();
            }
            // Live-font listener: re-push the resolved fonts on any
            // change to the four settings that feed
            // `resolveFontFamilies`. The `this.sourceUri` argument makes
            // `affectsConfiguration` honor per-folder overrides so a
            // change scoped to a different folder doesn't waste a
            // postMessage on this panel.
            if (
                instance.affectsAnyFontConfig(e)
            ) {
                void instance.pushFontFamilies();
            }
        });
        instance.perPanelDisposables.push(onTheme, onConfig);

        instance.updateContent({
            sourceUri: args.sourceUri,
            outputPath: args.outputPath,
            knitRootDir: args.knitRootDir,
        });
        return instance;
    }

    /**
     * Three-step preview-column recompute. Uses
     * `panel.viewColumn ?? lastKnownColumn` so a panel that is hidden
     * behind another tab (and therefore has `viewColumn === undefined`
     * per the VS Code API contract) still counts as occupying its
     * column.
     *
     *  - empty registry  → previewColumn = undefined
     *  - previewColumn still occupied by some panel → stays put
     *  - otherwise → adopts any surviving panel's column (so a
     *    dragged-away lone panel keeps siblings clustered with it)
     */
    private static recomputePreviewColumn(): void {
        if (KnitOutputPanel.instances.size === 0) {
            KnitOutputPanel.previewColumn = undefined;
            return;
        }
        const target = KnitOutputPanel.previewColumn;
        if (target !== undefined) {
            for (const inst of KnitOutputPanel.instances.values()) {
                const col = inst.panel.viewColumn ?? inst.lastKnownColumn;
                if (col === target) return;
            }
        }
        for (const inst of KnitOutputPanel.instances.values()) {
            const col = inst.panel.viewColumn ?? inst.lastKnownColumn;
            if (col !== undefined) {
                KnitOutputPanel.previewColumn = col;
                return;
            }
        }
        KnitOutputPanel.previewColumn = undefined;
    }

    private readonly context: vscode.ExtensionContext;

    private constructor(
        context: vscode.ExtensionContext,
        panel: vscode.WebviewPanel,
        rootDir: string,
        args: {
            sourceUri: vscode.Uri;
            outputPath: string;
            output: vscode.OutputChannel;
        },
    ) {
        this.context = context;
        this.panel = panel;
        this.rootDir = rootDir;
        this.sourceUri = args.sourceUri;
        this.outputPath = args.outputPath;
        this.output = args.output;

        // Prime the languageId cache from the open source document
        // (if any). `lookupSourceLanguageId`'s own side effect updates
        // `cachedSourceLanguageId` on every call, and the production
        // flow runs `updateContent` (which calls into the lookup)
        // immediately after this constructor — so this prime is
        // strictly redundant in production. It IS load-bearing for
        // direct unit-test callers that bypass `updateContent`: the
        // cache reflects the actual source languageId from the moment
        // the panel exists, not the `'rmd'` default.
        this.lookupSourceLanguageId();

        this.panel.webview.onDidReceiveMessage((msg: unknown) => this.handleMessage(msg));
    }

    private updateContent(args: {
        sourceUri: vscode.Uri;
        outputPath: string;
        knitRootDir?: string;
    }): void {
        this.sourceUri = args.sourceUri;
        this.outputPath = args.outputPath;
        const nonce = crypto.randomBytes(16).toString('base64');
        // Read the rendered HTML from disk; inlining via `srcdoc`
        // bypasses the nested-iframe navigation issue (see
        // buildShellHtml's doc comment).
        let htmlContent: string;
        try {
            htmlContent = fs.readFileSync(args.outputPath, 'utf-8');
        } catch (err) {
            this.output.appendLine(
                `[panel] read failed: ${err instanceof Error ? err.message : String(err)}`,
            );
            // Reached both when a live render's `.html` is unreadable and
            // when a restored panel has no artifact left to show. The
            // toolbar (including Knit again) is rendered by the
            // surrounding shell regardless, so point the user at it.
            htmlContent = '<!doctype html><html><body style="font-family: var(--vscode-font-family); '
                + 'padding: 1rem; color: var(--vscode-foreground);"><p>'
                + 'This preview is no longer available. Press <strong>Knit again</strong> '
                + '(the refresh button above) to regenerate it.'
                + '</p></body></html>';
        }
        // Subresources in the rendered HTML (CSS, images, fonts)
        // resolve relative to the document's directory. Setting the
        // base href to the webview URI for that directory makes those
        // requests go through the outer webview's resource handler.
        // The trailing slash is required so relative paths like
        // `img.png` resolve to `${dir}/img.png` rather than replacing
        // the last URL segment.
        const docDir = path.dirname(args.outputPath);
        const baseHref = this.panel.webview
            .asWebviewUri(vscode.Uri.file(docDir + path.sep))
            .toString();
        // Inline relative `<img>` sources as data URLs. VS Code's
        // resource handler does NOT intercept subresource fetches
        // issued from a nested `<iframe>` (the same restriction the
        // srcdoc workaround handles for top-level navigation), so the
        // `webview-resource://…/figure/plot-1.png` URL the `<base>`
        // resolves an `<img src>` to escapes the protocol handler and
        // hits the real network stack — yielding a broken-image icon
        // even though the file exists on disk and the browser-open
        // path renders it correctly. Inlining the bytes as
        // `data:image/png;base64,…` sidesteps the resource handler
        // entirely. The on-disk `.html` written by the post-knit
        // renderer still references images by relative path, so
        // "Open in Browser" stays self-contained without an inflated
        // base64 payload. The mutation only touches the in-memory
        // copy handed to the iframe.
        // `include_graphics()` can reference an image that already
        // lives elsewhere in the workspace (issue #627). Two things let
        // the inliner reach it:
        //   - `resolveBases`: an author's relative
        //     `include_graphics("images/logo.png")` is relative to the
        //     knit `root.dir`, not the preview output dir, so pass that
        //     base (falling back to the source document's own directory,
        //     which is `root.dir` in the default `document` mode and a
        //     sensible guess on a serializer restore with no re-knit).
        //   - `additionalRoots`: the containment allow-list — the
        //     workspace folder containing the source, plus `root.dir`
        //     itself in case it sits outside that folder.
        // Scoping to the containing workspace folder (not every open
        // folder) keeps the boundary to the project being previewed; the
        // inliner still `realpath`-guards containment.
        const sourceDir = path.dirname(this.sourceUri.fsPath);
        const rootBase = args.knitRootDir ?? sourceDir;
        const workspaceRoot = vscode.workspace.getWorkspaceFolder(this.sourceUri)?.uri.fsPath
            ?? sourceDir;
        htmlContent = inlineLocalImagesAsDataUrls(htmlContent, docDir, this.output, {
            markSvgPlots: true,
            resolveBases: [rootBase],
            additionalRoots: [workspaceRoot, rootBase],
        });
        this.panel.webview.html = buildShellHtml({
            htmlContent,
            baseHref,
            cspSource: this.panel.webview.cspSource,
            outputPath: args.outputPath,
            nonce,
            // Persisted into webview state for the serializer restore path.
            sourceFsPath: this.sourceUri.fsPath,
            // Persist the resolution base so a post-reload restore (which
            // has no knit to re-derive it) resolves relative
            // `include_graphics` images against the same directory.
            knitRootDir: args.knitRootDir,
            initialThemeApplied: KnitOutputPanel.readThemePreference(this.context),
            // Resolved palette is delivered out-of-band via
            // postMessage from `pushVscodeThemePalette` — the
            // initial value defaults to null so the webview boots
            // without a VS Code palette and falls back to the
            // GitHub variant until the resolve completes. This
            // avoids blocking the shell render on a theme JSON
            // read (which involves filesystem IO and grammar
            // priming).
            vscodeThemePaletteCss: null,
            // Resolved fonts ARE cheap (pure string handling, no IO)
            // so we compute them synchronously and bake the override
            // into the shell. This eliminates the single-frame flash
            // where the iframe would otherwise paint with the baked
            // (potentially-stale) fonts before the request/push round
            // trip lands. Falls back to null if resolution throws so
            // a panic here doesn't break the rest of the shell.
            vscodeFontFamiliesCss: this.resolveCurrentFontsCss(),
            // In a remote workspace, the toolbar's "Open in Browser"
            // action hands a `file://` URI to the extension-host
            // machine, which is the remote — not where the user is
            // sitting — so it cannot reach the local browser. The
            // shell omits the toolbar button and the right-click
            // menu item entirely; users reach the file via Export ▾,
            // which routes through the toast's remote-aware Download
            // flow. Same predicate the export toast in
            // `open-exported-file.ts` uses.
            isRemoteWorkspace:
                typeof vscode.env.remoteName === 'string'
                && vscode.env.remoteName.length > 0,
        });
        this.panel.title = `Knit Preview: ${path.basename(args.outputPath)}`;
        // Fire-and-forget: re-render the shell first, then resolve
        // and push the palette. The webview applies it as soon as
        // the message arrives (the initial render also runs
        // applyTheme(), so the toggle is visually responsive even
        // while we're still resolving).
        void this.pushVscodeThemePalette();
    }

    /**
     * Resolve the active VS Code theme's palette and push it into the
     * webview. Idempotent — repeated calls during rapid theme flips
     * or settings changes coalesce naturally because each postMessage
     * triggers the webview's `applyTheme` once.
     *
     * Returns silently on failure: the webview already falls back to
     * the GitHub-variant palette when no VS Code palette is set, so
     * a resolve failure is visually equivalent to the pre-feature
     * behavior. The first failure per panel session is logged to the
     * Knit output channel; subsequent failures are silent.
     */
    private async pushVscodeThemePalette(): Promise<void> {
        if (this.disposed) return;
        // Sequence-token cancellation. Several listeners (theme change,
        // configuration change, webview themeContext reply) can all
        // trigger a push in close succession; without sequencing the
        // resolves run in parallel and whichever finishes LAST wins.
        // The first push usually has `latestEditorBackground=undefined`
        // and picks the wrong candidate under autoDetect + same-kind
        // preferreds; if it finishes last, it overwrites the corrected
        // push and the user sees the wrong palette. Snapshot the
        // generation at entry and drop the result if a newer push has
        // started by the time we're ready to deliver.
        this.pushGeneration++;
        const myGeneration = this.pushGeneration;
        // The whole resolve path can throw if the extension context
        // is a sparse test stub (e.g. `{} as vscode.ExtensionContext`)
        // or if the cached grammar registry fails to init. Treat any
        // throw as "fallback to GitHub palette" — the webview already
        // handles a null css gracefully, and the toggle stays usable.
        let outcome: ThemePaletteOutcome;
        try {
            outcome = await this.resolveCurrentPalette();
        } catch (err) {
            outcome = {
                ok: false,
                reason: 'read-error',
                detail: err instanceof Error ? err.message : String(err),
            };
        }
        if (this.disposed) return;
        if (myGeneration !== this.pushGeneration) return;
        let css: string | null = null;
        if (outcome.ok) {
            css = paletteCssDeclarations(outcome.palette);
            // Log when the resolved theme id CHANGES, OR when the set
            // of candidate failures changes for the same theme — a
            // user breaking a non-active candidate (typo'd
            // `tokenColorCustomizations`, broken include path, etc.)
            // should still surface so they can diagnose the
            // misconfiguration. Dedup-by-key prevents the channel
            // filling up on body-class flips that re-resolve to the
            // same theme with the same failure profile.
            const failuresKey = serializeCandidateFailures(outcome.candidateFailures);
            const themeChanged = this.lastLoggedThemeId !== outcome.themeId;
            const failuresChanged = this.lastLoggedCandidateFailuresKey !== failuresKey;
            if (themeChanged || failuresChanged) {
                this.lastLoggedThemeId = outcome.themeId;
                this.lastLoggedCandidateFailuresKey = failuresKey;
                const root = vscode.workspace.getConfiguration();
                const kind = vscode.window.activeColorTheme.kind;
                const kindName =
                    kind === vscode.ColorThemeKind.Light ? 'Light'
                    : kind === vscode.ColorThemeKind.Dark ? 'Dark'
                    : kind === vscode.ColorThemeKind.HighContrast ? 'HighContrast'
                    : 'HighContrastLight';
                const isLight =
                    kind === vscode.ColorThemeKind.Light
                    || kind === vscode.ColorThemeKind.HighContrastLight;
                this.output.appendLine(
                    `[theme] inputs: activeColorTheme.kind=${kindName}, ` +
                    `autoDetectColorScheme=${root.get('window.autoDetectColorScheme')}, ` +
                    `workbench.colorTheme=${JSON.stringify(root.get('workbench.colorTheme'))}, ` +
                    `preferredLight=${JSON.stringify(root.get('workbench.preferredLightColorTheme'))}, ` +
                    `preferredDark=${JSON.stringify(root.get('workbench.preferredDarkColorTheme'))}, ` +
                    `candidates=${JSON.stringify(KnitOutputPanel.candidateThemeIds(isLight))}, ` +
                    `activeEditorBackground=${JSON.stringify(this.latestEditorBackground)}`,
                );
                for (const f of outcome.candidateFailures) {
                    this.output.appendLine(
                        `[theme] candidate "${f.themeId}" failed (${f.reason}): ${f.detail}`,
                    );
                }
                this.output.appendLine(
                    `[theme] resolved palette for "${outcome.themeId}" ` +
                    `(isLight=${outcome.isLight}): ${JSON.stringify(outcome.palette)}`,
                );
            }
        } else if (!this.themeResolveWarned) {
            this.themeResolveWarned = true;
            this.output.appendLine(
                `[theme] could not resolve VS Code theme palette (${outcome.reason}): ${outcome.detail}. ` +
                'Falling back to GitHub palette.',
            );
        }
        const message: VscodeThemePaletteUpdate = {
            __ravenVscodeThemePalette: true,
            css,
        };
        try {
            await this.panel.webview.postMessage(message);
        } catch (err) {
            // postMessage can reject if the panel was disposed
            // between our check and the call — that's not a real
            // error, just lost-update.
            if (this.disposed) return;
            this.output.appendLine(
                `[theme] postMessage failed: ${err instanceof Error ? err.message : String(err)}`,
            );
        }
    }

    /**
     * Read the current VS Code state and resolve the active theme's
     * palette. Production wiring lives here so the extractor stays
     * pure (and unit-testable without spinning up VS Code).
     *
     * The grammar registry is shared with the post-knit renderer's
     * cached instance; reusing it amortizes the vscode-textmate +
     * onig.wasm initialisation across the panel and the renderer.
     */
    private async resolveCurrentPalette(): Promise<ThemePaletteOutcome> {
        const registry = getKnitGrammarRegistry(this.context);
        const editor = vscode.workspace.getConfiguration('editor');
        const kind = vscode.window.activeColorTheme.kind;
        const isLight =
            kind === vscode.ColorThemeKind.Light
            || kind === vscode.ColorThemeKind.HighContrastLight;
        return resolveActiveThemePalette({
            candidateThemeIds: KnitOutputPanel.candidateThemeIds(isLight),
            activeEditorBackground: this.latestEditorBackground,
            isLight,
            extensions: vscode.extensions.all.map((e) => ({
                id: e.id,
                extensionPath: e.extensionPath,
                packageJSON: e.packageJSON,
            })),
            tokenColorCustomizations: editor.get('tokenColorCustomizations'),
            semanticTokenColorCustomizations: editor.get('semanticTokenColorCustomizations'),
            registry,
            readFile: (absPath) => fs.promises.readFile(absPath, 'utf-8'),
            realPath: (absPath) => fs.promises.realpath(absPath),
        });
    }

    /**
     * Build the ordered list of candidate theme ids the resolver
     * will try. The first whose theme JSON's `colors.editor.background`
     * matches the webview's actual `--vscode-editor-background` wins;
     * if there's no match (or the webview hasn't reported a bg yet),
     * the first candidate is used.
     *
     * Why a list rather than a single id: when
     * `window.autoDetectColorScheme: true` and the user has both
     * `preferredLightColorTheme` and `preferredDarkColorTheme`
     * configured to themes with the same kind (e.g. both dark), the
     * public API can't tell us which one VS Code is actually
     * rendering. `activeColorTheme.kind` returns the chosen theme's
     * type, not the OS appearance. Providing both as candidates and
     * letting the resolver disambiguate by editor.background is the
     * only public-API path that gets this right.
     *
     * Ordering: kind-matching preferred-* first (so the typical
     * "preferredLight is a light theme, preferredDark is a dark
     * theme" case works even without a bg hint), then the other
     * preferred-*, then `workbench.colorTheme` as a last fallback.
     */
    private static candidateThemeIds(isLight: boolean): readonly string[] {
        const root = vscode.workspace.getConfiguration();
        const autoDetect = root.get<boolean>('window.autoDetectColorScheme', false);
        const candidates: string[] = [];

        if (autoDetect) {
            const prefLight = root.get<string>('workbench.preferredLightColorTheme');
            const prefDark = root.get<string>('workbench.preferredDarkColorTheme');
            const first = isLight ? prefLight : prefDark;
            const second = isLight ? prefDark : prefLight;
            if (typeof first === 'string' && first.length > 0) candidates.push(first);
            if (
                typeof second === 'string'
                && second.length > 0
                && !candidates.includes(second)
            ) {
                candidates.push(second);
            }
        }

        const colorTheme = vscode.workspace
            .getConfiguration('workbench')
            .get<string>('colorTheme', '');
        if (typeof colorTheme === 'string' && colorTheme.length > 0 && !candidates.includes(colorTheme)) {
            candidates.push(colorTheme);
        }

        return candidates;
    }

    /**
     * Storage key for the "Apply VS Code theme" toggle. Lives in
     * `globalState` so the choice persists across panel disposal /
     * recreation, across knits, and across VS Code restarts.
     */
    private static readonly THEME_PREFERENCE_KEY = 'raven.knit.applyVSCodeTheme';

    private static readThemePreference(context: vscode.ExtensionContext): boolean {
        // `globalState` is undefined in some test paths that stub
        // ExtensionContext with `{}`; treat that the same as
        // "preference not yet stored" rather than crashing.
        const gs = context.globalState as vscode.Memento | undefined;
        if (!gs || typeof gs.get !== 'function') return false;
        const v = gs.get<unknown>(KnitOutputPanel.THEME_PREFERENCE_KEY);
        return typeof v === 'boolean' ? v : false;
    }

    private handleMessage(msg: unknown): void {
        if (!isKnitOutputMessage(msg)) return;
        if (msg.type === 'refresh') {
            void vscode.commands.executeCommand('raven.knit', this.sourceUri);
            return;
        }
        if (msg.type === 'openInBrowser') {
            void openInBrowser(this.outputPath, this.output);
            return;
        }
        if (msg.type === 'themeChanged') {
            void this.context.globalState.update(
                KnitOutputPanel.THEME_PREFERENCE_KEY,
                msg.applied,
            );
            return;
        }
        if (msg.type === 'themeContext') {
            // The webview just reported its actually-rendered
            // `--vscode-editor-background`. Update our cached value
            // and re-resolve the palette so the disambiguation by
            // editor.background can pick the right theme JSON among
            // the candidates. Skip if unchanged to avoid extra work
            // on every body-class flip.
            const bg = msg.editorBackground.trim().toLowerCase();
            const RAVEN_HEX = /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6,8})$/i;
            if (!RAVEN_HEX.test(bg)) {
                // The disambiguation path silently failing because
                // VS Code emitted (or the user customized) the bg in
                // an `rgb()` / `rgba()` / named-color form leaves no
                // user-visible signal. Log on transitions so the
                // output channel carries the diagnostic — without
                // spamming on every body-class flip that re-reports
                // the same rejected value.
                if (this.lastRejectedEditorBackground !== bg) {
                    this.lastRejectedEditorBackground = bg;
                    this.output.appendLine(
                        `[theme] webview reported a non-hex editor.background ` +
                        `(${JSON.stringify(msg.editorBackground)}); ` +
                        `disambiguation-by-bg is disabled until a hex value arrives.`,
                    );
                }
                return;
            }
            this.lastRejectedEditorBackground = undefined;
            if (this.latestEditorBackground === bg) return;
            this.latestEditorBackground = bg;
            void this.pushVscodeThemePalette();
            return;
        }
        if (msg.type === 'requestPalette') {
            // The webview booted (initial load or panel reuse) and is
            // asking us for the current palette. Push without
            // invalidating `latestEditorBackground` — we want the new
            // shell to receive whatever palette matches the cached bg.
            // The shell will follow up with its own `themeContext`
            // shortly; if that report's bg differs, the resulting
            // re-resolve will land later and (thanks to pushGeneration)
            // supersedes this one cleanly.
            void this.pushVscodeThemePalette();
            return;
        }
        if (msg.type === 'requestFonts') {
            // Same pattern as `requestPalette` — the fresh shell pulls
            // the current font CSS, closing the panel-reuse race where
            // a host-initiated push could land before the new
            // listener is wired up.
            void this.pushFontFamilies();
            return;
        }
        if (msg.type === 'requestExport') {
            void this.runExportFromWebview(msg.format);
            return;
        }
        if (msg.type === 'cancelExport') {
            // Cancellation is routed via the OperationRegistry that
            // `export-commands.ts` owns. We dispatch the dedicated
            // command so the panel doesn't need a direct registry
            // reference; the export module is the single source of
            // truth for "is there a running export and how do I cancel
            // it." The command is no-op if there's no in-flight export.
            void vscode.commands.executeCommand('raven.knit.cancelExport', this.sourceUri);
            return;
        }
    }

    /**
     * Dispatch the appropriate `raven.knit.export*` command for the
     * format the user picked in the webview's Export popover.
     *
     * The webview owns the format-choice UI now (an HTML popover
     * mirroring the plot viewer's share popover), so VS Code's native
     * QuickPick is no longer in the path. The format value arrives
     * across the trust boundary in `requestExport.format` and is
     * already validated against `EXPORT_FORMATS` by
     * `isKnitOutputMessage` — we still re-narrow here to a strict
     * switch so the dispatch table is exhaustive at the type level.
     *
     * The preview dir is pinned across the dispatch lifecycle. Without
     * this, the user could click a format and close the panel before
     * the export starts — the disposal handler would have already
     * requested deletion of the cached `.md`, and the export pipeline
     * would find it gone. The export pipeline takes its own pin when
     * it begins, so any window between our unpin and that pin would
     * re-open the race; we hold our pin until `executeCommand`
     * resolves (i.e., until the export pipeline has finished and
     * already done its own pin/unpin cycle).
     */
    private async runExportFromWebview(
        format: 'html' | 'pdf' | 'docx',
    ): Promise<void> {
        let command: 'raven.knit.exportHtml' | 'raven.knit.exportPdf' | 'raven.knit.exportDocx';
        switch (format) {
            case 'html': command = 'raven.knit.exportHtml'; break;
            case 'pdf':  command = 'raven.knit.exportPdf';  break;
            case 'docx': command = 'raven.knit.exportDocx'; break;
            // Defense-in-depth: TS already narrows `format` to the three
            // literals above, but if `EXPORT_FORMATS` is ever widened in
            // `knit-output.ts` without updating this dispatch, an
            // exhaustiveness assignment to `never` makes the drift loud
            // at compile time AND throws a clear error at runtime.
            default: {
                const _exhaustive: never = format;
                throw new Error(`unsupported export format: ${String(_exhaustive)}`);
            }
        }
        const rmdAbsPath = this.sourceUri.fsPath;
        const pin = KnitOutputPanel.pinPreviewHandler;
        const unpin = KnitOutputPanel.unpinPreviewHandler;
        if (pin) pin(rmdAbsPath);
        try {
            // The webview entry reuses the cached preview .md. We dispatch
            // through `raven.knit.export*` so any caller-supplied wiring
            // (test harness, etc.) gets the same entry point as the
            // editor-toolbar invocations — `runExport` then differentiates
            // entry-mode by the optional second argument.
            await vscode.commands.executeCommand(command, this.sourceUri, { entry: 'webview' });
        } finally {
            if (unpin) unpin(rmdAbsPath);
        }
    }

    /**
     * Resolve the current font settings for this panel's source URI
     * and push them into the webview as a `__ravenFontFamilies` CSS
     * declaration string. Mirrors `pushVscodeThemePalette` shape but
     * has no generation counter — font resolution is synchronous and
     * idempotent, so the last call wins naturally.
     *
     * Returns silently on failure: the webview already falls back to
     * the fonts baked into the on-disk `.html` when no override is
     * set, so a resolve failure is visually equivalent to the
     * pre-feature behavior.
     */
    private async pushFontFamilies(): Promise<void> {
        if (this.disposed) return;
        let css: string | null;
        try {
            css = fontsCssDeclarations(this.resolveCurrentFonts());
        } catch (err) {
            this.output.appendLine(
                `[fonts] resolve failed: ${err instanceof Error ? err.message : String(err)}`,
            );
            css = null;
        }
        if (this.disposed) return;
        const message: FontFamiliesUpdate = {
            __ravenFontFamilies: true,
            css,
        };
        try {
            await this.panel.webview.postMessage(message);
        } catch (err) {
            if (this.disposed) return;
            this.output.appendLine(
                `[fonts] postMessage failed: ${err instanceof Error ? err.message : String(err)}`,
            );
        }
    }

    /**
     * Synchronous helper that reads the current font configuration for
     * this panel's source URI (and the source's languageId, for
     * `[rmd]`/`[quarto]`/`[markdown]` language-scoped
     * `editor.fontFamily` overrides) and returns the resolved fonts.
     *
     * Source languageId is read live from the open `TextDocument` if
     * any; we fall back to `'rmd'` (the only languageId `Raven: Knit Preview`
     * acts on today) when the document is closed. The fallback covers
     * the case where the user closes the .Rmd buffer while the
     * preview panel is still open — they'd otherwise see a font
     * mismatch on the next setting change.
     */
    private resolveCurrentFonts(): ResolvedFonts {
        const languageId = this.lookupSourceLanguageId();
        const knitConfig = vscode.workspace.getConfiguration('raven.knit', this.sourceUri);
        const mdPreviewConfig = vscode.workspace.getConfiguration('markdown.preview', this.sourceUri);
        const editorConfig = vscode.workspace.getConfiguration(
            'editor',
            { uri: this.sourceUri, languageId },
        );
        return resolveFontFamilies(
            knitConfig.get<string>('fontFamily', ''),
            knitConfig.get<string>('monospaceFontFamily', ''),
            mdPreviewConfig.get<string>('fontFamily', ''),
            editorConfig.get<string>('fontFamily', ''),
        );
    }

    private lookupSourceLanguageId(): string {
        const fsPath = this.sourceUri.fsPath;
        for (const doc of vscode.workspace.textDocuments) {
            if (doc.uri.fsPath === fsPath) {
                // Cache the languageId so it persists after the document closes
                this.cachedSourceLanguageId = doc.languageId;
                return doc.languageId;
            }
        }
        return this.cachedSourceLanguageId;
    }

    /**
     * Synchronous helper used by `updateContent` to bake the live font
     * override into the shell HTML on first paint. Mirrors what
     * `pushFontFamilies` does over postMessage but produces the CSS
     * string directly. Falls back to null on any failure so the shell
     * still renders.
     */
    private resolveCurrentFontsCss(): string | null {
        try {
            return fontsCssDeclarations(this.resolveCurrentFonts());
        } catch (err) {
            this.output.appendLine(
                `[fonts] initial resolve failed: ${err instanceof Error ? err.message : String(err)}`,
            );
            return null;
        }
    }

    /**
     * Returns `true` when a configuration change affects any of the
     * four settings `resolveFontFamilies` reads — `raven.knit.fontFamily`,
     * `raven.knit.monospaceFontFamily`, `markdown.preview.fontFamily`,
     * `editor.fontFamily` — scoped to this panel's source URI AND
     * the source document's languageId so a change scoped to a
     * different folder or a different language (e.g. `[markdown]:
     * editor.fontFamily` while this panel is on an `.rmd`) does NOT
     * trigger a postMessage here.
     *
     * The languageId match matters for `editor.fontFamily`: a user
     * who sets `[r]` and `[rmd]` to different fonts should see only
     * their `[rmd]` change reach this panel when the source is an
     * `.Rmd`. Passing only the URI lets `affectsConfiguration` apply
     * the broader resource-scope check, which can fire spuriously for
     * unrelated language overrides.
     */
    private affectsAnyFontConfig(e: vscode.ConfigurationChangeEvent): boolean {
        const scope: vscode.ConfigurationScope = {
            uri: this.sourceUri,
            languageId: this.lookupSourceLanguageId(),
        };
        return e.affectsConfiguration('raven.knit.fontFamily', scope)
            || e.affectsConfiguration('raven.knit.monospaceFontFamily', scope)
            || e.affectsConfiguration('markdown.preview.fontFamily', scope)
            || e.affectsConfiguration('editor.fontFamily', scope);
    }
}

/**
 * Build a stable signature for the `candidateFailures` list so we can
 * detect changes without retaining the array itself. JSON.stringify
 * over the relevant fields is order-preserving and deterministic for
 * our purposes.
 */
function serializeCandidateFailures(
    failures: ReadonlyArray<{ themeId: string; reason: string; detail: string }>,
): string {
    return JSON.stringify(failures.map((f) => [f.themeId, f.reason, f.detail]));
}

/**
 * Open the rendered file via the user's OS default browser.
 *
 * In local workspaces this opens the configured handler for `file:` (a
 * browser, typically). In remote workspaces, `openExternal(file:)` may
 * route the request to the extension-host machine — i.e. the remote
 * server, not where the user is sitting. When `openExternal` returns
 * false we write the path to the Knit output channel and warn the user.
 */
export async function openInBrowser(
    outputPath: string,
    output: vscode.OutputChannel,
): Promise<void> {
    const uri = vscode.Uri.file(outputPath);
    let opened = false;
    try {
        opened = await vscode.env.openExternal(uri);
    } catch (err) {
        output.appendLine(
            `[Open in Browser] openExternal threw: ${err instanceof Error ? err.message : String(err)}`,
        );
    }
    if (opened) return;
    output.appendLine(`[Open in Browser] file:// did not open. Rendered output is at: ${outputPath}`);
    void vscode.window.showWarningMessage(
        'Open in Browser is not available for this workspace. The rendered file path has been written to the Raven: Knit output channel.',
    );
}
