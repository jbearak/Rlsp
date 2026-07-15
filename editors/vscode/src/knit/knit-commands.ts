import * as path from 'path';
import * as vscode from 'vscode';
import {
    Blocker,
    detectBlockers,
    detectFormat,
    extractFrontmatter,
    parseFrontmatter,
} from './yaml-frontmatter';
import {
    applyKnitTargetChunkDefaults,
    parseOutputOptions,
    type TargetFormat,
} from './output-options';
import {
    buildKnitExpression,
    escapeRString,
    ValidateFormatError,
    ValidatePathError,
} from './r-expression';
import { runKnit } from './knit-engine';
import { computeHtmlOutputPath } from './knit-paths';
import { canonicalOpKey, previewArtifactPaths } from './raven-knit-paths';
import { OperationRegistry, type OperationController } from './operation-controller';
import * as fs from 'fs';
import { runPostKnitRender } from './post-knit-renderer';
import type { LanguageClient } from 'vscode-languageclient/node';
import { resolveRConsoleActivation } from '../r-console-activation';
import { KnitOutputPanel } from './knit-output-panel';
import {
    classify,
    pickPrimaryOutput,
    type KnitOutcome,
} from './knit-output';

const OUTPUT_CHANNEL_NAME = 'Raven: Knit';
const DEFAULT_TIMEOUT_MS = 600_000;

type WorkingDirectoryMode = 'document' | 'project' | 'current';

/**
 * Resolved dependency surface used throughout the knit command. The
 * fields are required at the point of use; the public optional shape
 * (`Partial<KnitDeps>` parameter on `registerKnitCommands`) lets tests
 * override individual functions while production omits the parameter
 * entirely.
 */
export interface KnitDeps {
    runKnit: typeof runKnit;
    showOrUpdatePanel: typeof KnitOutputPanel.showOrUpdate;
    /**
     * The live LSP client used by `runPostKnitRender` to fetch
     * Raven's `function` semantic tokens. `undefined` is tolerated
     * (the renderer falls back to grammar-only highlighting).
     */
    getLanguageClient: () => LanguageClient | undefined;
    /**
     * Post-knit render step. Defaults to `runPostKnitRender`; tests
     * override to avoid touching the filesystem and the markdown API.
     */
    runPostKnitRender: typeof runPostKnitRender;
    /**
     * Optional shared "Raven: Knit" output channel. When omitted,
     * `registerKnitCommands` creates and owns one. When provided
     * (production calls it that way from `knit/index.ts`), the same
     * channel is used by both knit and Pandoc export logs so the
     * "Show Knit Output" command reveals a single source of truth.
     */
    sharedOutput?: vscode.OutputChannel;
    /**
     * Optional shared OperationRegistry. When provided (production
     * calls it that way), knit and export ops share one per-source
     * gate so an in-flight knit blocks export and vice versa. When
     * omitted, `registerKnitCommands` creates a private registry —
     * legacy / test-only behavior.
     */
    sharedRegistry?: OperationRegistry;
}

/**
 * Top-level registration. Creates the lazy OutputChannel (unless one
 * is injected via `deps.sharedOutput`) and registers the two commands
 * listed in `package.json`.
 *
 * `sharedOutput` lets callers (currently `knit/index.ts`) inject one
 * "Raven: Knit" channel shared with the export pipeline, so both knit
 * and Pandoc logs appear in the same place that `raven.knit.openOutputChannel`
 * reveals.
 */
export function registerKnitCommands(
    context: vscode.ExtensionContext,
    deps?: Partial<KnitDeps>,
): vscode.OutputChannel {
    const resolved: KnitDeps = {
        runKnit: deps?.runKnit ?? runKnit,
        showOrUpdatePanel: deps?.showOrUpdatePanel ?? KnitOutputPanel.showOrUpdate,
        getLanguageClient: deps?.getLanguageClient ?? (() => undefined),
        runPostKnitRender: deps?.runPostKnitRender ?? runPostKnitRender,
    };

    let outputChannel: vscode.OutputChannel | undefined = deps?.sharedOutput;
    const getOutput = (): vscode.OutputChannel => {
        if (!outputChannel) {
            outputChannel = vscode.window.createOutputChannel(OUTPUT_CHANNEL_NAME);
            context.subscriptions.push(outputChannel);
        }
        return outputChannel;
    };

    // Per-source operation registry. Shared with the export pipeline
    // when `deps.sharedRegistry` is supplied (production path). The
    // shared registry ensures an in-flight knit blocks a same-source
    // export and vice versa. When omitted (tests), we keep a private
    // registry per `registerKnitCommands` call.
    const registry = deps?.sharedRegistry ?? new OperationRegistry();

    context.subscriptions.push(
        vscode.commands.registerCommand(
            'raven.knit',
            async (uri?: vscode.Uri) => {
                await runKnitCommand(uri, getOutput(), registry, context, resolved);
            },
        ),
        vscode.commands.registerCommand(
            'raven.knit.openOutputChannel',
            () => getOutput().show(true),
        ),
    );

    return getOutput();
}

/**
 * Public re-entry point for callers that already hold a controller
 * slot on this source (the editor-toolbar export pipeline calls this
 * after taking out an `export-*` controller, so re-acquiring through
 * `beginOp` would falsely report "busy"). The caller MUST already
 * own the registry slot for the source URI; otherwise use the
 * registered command surface instead.
 *
 * The `externalController` is the caller's already-running operation
 * controller. We bridge its `cancelled` flag into the knit subprocess
 * via a CancellationTokenSource so cancelling the outer export
 * actually stops the underlying R subprocess. We also skip the
 * nested `withProgress` — the outer caller already shows progress;
 * a second notification would clutter the UX.
 *
 * `targetFormat` (default `'html'`) selects which YAML `output:` block
 * supplies chunk options for the knit. Editor-toolbar Export → PDF /
 * Word passes `'pdf'` / `'docx'` so target-specific options like
 * `pdf_document: { fig_width: 4, dpi: 300 }` actually shape the
 * figures produced for that target. Without this, PDF/Word exports
 * would knit with HTML chunk options and Pandoc would convert
 * HTML-sized figures into the wrong target — violating the design
 * intent in docs/superpowers/specs/2026-05-23-knit-preview-export-design.md.
 */
export async function runKnitWithExistingController(
    explicitUri: vscode.Uri | undefined,
    output: vscode.OutputChannel,
    context: vscode.ExtensionContext,
    deps: KnitDeps,
    externalController: OperationController,
    targetFormat: TargetFormat = 'html',
): Promise<{ ok: boolean }> {
    // Capture the outcome via the dedicated channel `runKnitCommand`
    // populates when `outcomeCapture` is non-null. Avoids refactoring
    // every return path in runKnitCommand (which spans validation
    // gates, working-dir resolution, R-expression construction, etc.)
    // into a typed return.
    const capture: { outcome: KnitOutcome | null } = { outcome: null };
    await runKnitCommand(
        explicitUri,
        output,
        /* registry */ null,
        context,
        deps,
        externalController,
        capture,
        targetFormat,
    );
    return { ok: capture.outcome !== null && capture.outcome.kind === 'ok' };
}

async function runKnitCommand(
    explicitUri: vscode.Uri | undefined,
    output: vscode.OutputChannel,
    registry: OperationRegistry | null,
    context: vscode.ExtensionContext,
    deps: KnitDeps,
    externalController: OperationController | null = null,
    /**
     * Optional out-channel for the final KnitOutcome. The export
     * pipeline uses this to distinguish a successful knit from a
     * cancelled/failed one that left a partial `.md`. Null means
     * "don't capture" — the existing command-surface call site
     * leaves the field unset.
     */
    outcomeCapture: { outcome: KnitOutcome | null } | null = null,
    /**
     * Which YAML `output:` block supplies chunk options for the knit.
     * Default `'html'` matches Knit Preview's HTML-always contract.
     * Editor-toolbar Export → PDF / Word passes `'pdf'` / `'docx'`
     * so target-specific YAML like `pdf_document: { fig_width: 4,
     * dpi: 300 }` actually drives the knit instead of being ignored
     * in favour of `html_document`'s defaults.
     */
    targetFormat: TargetFormat = 'html',
): Promise<void> {
    const docUri = explicitUri ?? vscode.window.activeTextEditor?.document.uri;
    if (!docUri) {
        await vscode.window.showInformationMessage(
            'Raven: Knit Preview requires an active editor with an R Markdown file.',
        );
        return;
    }

    // Re-check the *resolved* gate. The command-palette `when` clauses
    // already gate on `raven.rmdKnit.enabled`, but the command itself
    // is registered unconditionally (so user keybindings and
    // `tasks.json` entries keep working), and a stale auto-resolution
    // after REditorSupport is enabled would otherwise let knit run.
    if (resolveRConsoleActivation() !== 'enabled') {
        await vscode.window.showInformationMessage(
            'Raven: Knit Preview is disabled by your `raven.rConsole.activation` setting (or because REditorSupport / Positron is active).',
        );
        return;
    }

    // Reject inputs that aren't file-backed R Markdown documents. Order
    // matters: an untitled buffer with `languageId === 'rmd'` has a
    // URI scheme of `untitled` and a path without an extension; we
    // surface "save the file first" rather than the misleading
    // "not an R Markdown file" message. The AGENTS.md "File-type tracking"
    // learning calls this out specifically.
    if (docUri.scheme !== 'file' && docUri.scheme !== 'vscode-remote') {
        await vscode.window.showInformationMessage(
            'Save the file to disk before running Raven: Knit Preview.',
        );
        return;
    }

    if (!vscode.workspace.isTrusted) {
        const MANAGE = 'Manage Workspace Trust';
        const choice = await vscode.window.showInformationMessage(
            'Raven: Knit Preview is disabled in untrusted workspaces.',
            MANAGE,
        );
        if (choice === MANAGE) {
            await vscode.commands.executeCommand('workbench.trust.manage');
        }
        return;
    }

    // After the scheme check passes we know we have a file-backed URI.
    const ext = path.extname(docUri.fsPath || docUri.path).toLowerCase();
    if (ext !== '.rmd' && ext !== '.rmarkdown') {
        await vscode.window.showInformationMessage(
            'Raven: Knit Preview only runs on .Rmd or .Rmarkdown files.',
        );
        return;
    }

    const fsPath = docUri.fsPath;
    if (!fsPath) {
        await vscode.window.showInformationMessage(
            'Save the file to disk before running Raven: Knit Preview.',
        );
        return;
    }

    let documentText: string;
    let sourceLanguageId: string;
    try {
        const doc = await vscode.workspace.openTextDocument(docUri);
        // knitr reads the .Rmd from disk via R's `readLines`, not from
        // VS Code's in-memory buffer. If the editor has unsaved
        // changes, the knit output would silently reflect the
        // stale-on-disk version — which is indistinguishable from "the
        // knit didn't work" from the user's perspective. Save before
        // running so the disk and the buffer agree.
        //
        // `save()` returns false if a participant (formatter,
        // codeActionsOnSave, etc.) refuses the save. In that case we
        // can't know whether the disk reflects the user's intent, so
        // surface the failure and bail rather than knit a stale file.
        if (doc.isDirty) {
            let saved = false;
            try {
                saved = await doc.save();
            } catch (err) {
                output.show(true);
                output.appendLine(
                    `[knit] save failed for ${fsPath}: ` +
                    (err instanceof Error ? err.message : String(err)),
                );
            }
            if (!saved) {
                await vscode.window.showWarningMessage(
                    `Raven: Knit Preview — could not save ${path.basename(fsPath)}. ` +
                    `The knit output would not reflect your unsaved changes.`,
                );
                return;
            }
        }
        documentText = doc.getText();
        sourceLanguageId = doc.languageId;
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        await vscode.window.showErrorMessage(`Raven: Knit Preview could not read document: ${message}`);
        return;
    }

    // [2] Parse YAML front matter.
    const fmText = extractFrontmatter(documentText) ?? '';
    const parsed = parseFrontmatter(fmText);
    if (!parsed.ok) {
        output.show(true);
        output.appendLine(`[YAML parse error] ${parsed.error}`);
        await vscode.window.showWarningMessage(
            'Raven: Knit Preview — YAML front matter is malformed; see Raven: Knit output.',
        );
        return;
    }

    // [3] Detect deferred-feature blockers.
    const blockers = detectBlockers(parsed.value);
    if (blockers.length > 0) {
        await showBlocker(blockers[0], fsPath);
        return;
    }

    // [4] Format detection.
    //
    // Knit Preview ignores the YAML `output:` block when deciding how
    // to render — we always produce an HTML preview into the per-
    // session temp dir, regardless of `output: pdf_document`,
    // `output: word_document`, etc. The format identifier is still
    // computed (and passed through validation downstream) for logging,
    // but no longer gates execution.
    //
    // Why ignore it? `knitr::knit` doesn't read the `output:` block —
    // that's an rmarkdown concept consumed by `rmarkdown::render`.
    // Honoring it would require switching to rmarkdown (requires
    // Pandoc on the preview path) and losing Raven's TextMate-based
    // syntax highlighting + theme overlay. Trade-off documented in
    // the design spec at docs/superpowers/specs/2026-05-23-knit-preview-export-design.md.
    const format = detectFormat(parsed.value);

    // [5] Resolve working directory.
    const workingDirectoryMode = vscode.workspace
        .getConfiguration('raven.knit')
        .get<WorkingDirectoryMode>('workingDirectory', 'document');
    const knitDirResult = resolveKnitDir(docUri, workingDirectoryMode);
    if (!knitDirResult.ok) {
        await vscode.window.showErrorMessage(knitDirResult.error);
        return;
    }
    const { knitRootDir, cwd } = knitDirResult;

    // [6] Build R expression.
    // Resolve the per-session temp paths now. Knit Preview writes its
    // intermediate `.md`, final `.html`, and `figure/` artifacts into
    // <tmpdir>/raven-knit/<workspaceHash>/<sessionId>/preview/<sourceHash>/
    // — never next to the source `.Rmd`. We must ensure the directory
    // exists before R runs, since knitr won't `mkdir -p` for us.
    const previewPaths = previewArtifactPaths(fsPath);
    const mdOutputPath = previewPaths.mdPath;

    // Pin the preview dir as soon as it's known and before any await
    // that touches it. `mkdir`, the busy information toast, and the
    // knit subprocess itself all yield the event loop; without this
    // early pin a panel-disposal `requestPreviewDirDeletion` fired
    // during any of those awaits would see refcount = 0 and schedule
    // the unrecoverable `fs.rm -rf` against the dir we're about to
    // write into. (Earlier awaits in this function — `openTextDocument`,
    // `save`, the YAML-parsing path — operate on the source `.Rmd`,
    // not the preview dir, so they're outside the race surface.) The
    // pin must guard the entire knit + post-knit render pipeline.
    //
    // Re-entry path: when `registry === null` the export pipeline
    // owns the pin via `runExport`, so we skip — double-pinning would
    // leave the refcount stuck above zero after both pipelines unpin.
    let pinnedPreviewKey: string | null = null;
    if (registry !== null) {
        registry.pinPreviewDir(previewPaths.previewKey);
        pinnedPreviewKey = previewPaths.previewKey;
    }

    let knitProducedValidMd = false;
    let controller: OperationController | null = null;
    try {
        try {
            await fs.promises.mkdir(previewPaths.previewDir, { recursive: true });
        } catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            output.show(true);
            output.appendLine(`[knit] Failed to create temp dir ${previewPaths.previewDir}: ${message}`);
            await vscode.window.showErrorMessage(
                `Raven: Knit Preview could not create temp directory. See output for details.`,
            );
            return;
        }

        // YAML output: block — chunk-level options come from here. The
        // preview path always uses 'html' (preview renders to HTML
        // regardless of YAML output:). The editor-toolbar export path
        // passes its target format so PDF / Word knits pick chunk options
        // from `pdf_document:` / `word_document:` instead of
        // `html_document:`. Per docs/superpowers/specs/2026-05-23-knit-preview-export-design.md,
        // non-matching format blocks are completely ignored: when target
        // is 'pdf', `html_document.fig_width` does NOT drive the PDF
        // knit, and vice versa.
        const outputOpts = parseOutputOptions(parsed.value, targetFormat);
        const chunkOpts = applyKnitTargetChunkDefaults(outputOpts.chunkOpts, targetFormat);
        if (outputOpts.ignored.length > 0) {
            for (const key of outputOpts.ignored) {
                output.appendLine(`[knit] Ignored output: option '${key}'`);
            }
        }

        let expression: string;
        try {
            expression = buildKnitExpression({
                filePath: fsPath,
                outputPath: mdOutputPath,
                format,
                knitRootDir,
                // base.dir is the preview temp dir so knitr's plots land
                // under <previewDir>/figure/ alongside the .md. The
                // relative fig.path lets the .md reference figures as
                // `figure/<chunk>-N.png`, and Pandoc's `cwd` (set during
                // export to `previewDir`) resolves those relative paths
                // against the freshly-generated figures — never against
                // stale source-directory artifacts.
                baseDir: previewPaths.previewDir,
                figPath: 'figure/',
                chunkOpts,
            });
        } catch (err) {
            const isPathError = err instanceof ValidatePathError;
            const isFormatError = err instanceof ValidateFormatError;
            const message = err instanceof Error ? err.message : String(err);
            output.show(true);
            output.appendLine(`[validation] ${message}`);
            const surface = isFormatError
                ? `Raven: Knit Preview — unsupported output format identifier in YAML.`
                : isPathError
                    ? `Raven: Knit Preview — file path contains an unsupported character. See output for details.`
                    : `Raven: Knit Preview — validation failed. See output for details.`;
            await vscode.window.showErrorMessage(surface);
            return;
        }

        // [7] Spawn + [8] Stream + [9] Exit.
        const rBinary = resolveRBinary();
        const timeoutMs = readTimeoutMs();
        const baseName = path.basename(fsPath);

        // Concurrent-op guard. The shared OperationRegistry tracks both
        // knit-preview and export-* ops; a busy result here means either
        // another knit is in flight on the same source (the legacy case)
        // or an export is mid-Pandoc on this file (so the cached .md is
        // being consumed and a fresh knit would race). The canonical key
        // collapses different URI shapes of the same file (e.g., case
        // differences on Windows) onto a single slot.
        //
        // `registry === null` is the re-entry path: the editor-toolbar
        // export pipeline took out an `export-*` controller and now calls
        // this function to perform the underlying knit. The caller already
        // owns the slot; skipping the beginOp lets the nested knit proceed
        // without falsely reporting "already being knitted by the export
        // I just started".
        const opKey = canonicalOpKey(docUri);
        if (registry !== null) {
            const begin = registry.beginOp(opKey, 'knit-preview');
            if (begin.kind === 'busy') {
                const what =
                    begin.existing.kind === 'knit-preview'
                        ? 'being knitted'
                        : begin.existing.kind === 'export-html'
                            ? 'exporting to HTML'
                            : begin.existing.kind === 'export-pdf'
                                ? 'exporting to PDF'
                                : begin.existing.kind === 'export-docx'
                                    ? 'exporting to Word'
                                    : 'busy';
                await vscode.window.showInformationMessage(
                    `Raven: Knit Preview — ${baseName} is already ${what}.`,
                );
                return;
            }
            controller = begin.controller;
        }

        output.appendLine(`---`);
        output.appendLine(`Knitting ${fsPath}`);
        output.appendLine(`R: ${rBinary}`);
        output.appendLine(`Expression: ${expression}`);
        output.appendLine(`cwd: ${cwd}`);
        output.appendLine(``);

        let outcome: KnitOutcome;
        try {
            if (externalController !== null) {
                // Re-entry path: the export pipeline already owns a progress
                // notification and a registry slot. Skip a second `withProgress`
                // (a nested notification would clutter the UX) and bridge the
                // export controller's `cancelled` flag into the R subprocess
                // via a CancellationTokenSource. Polling once per 100ms is
                // cheap and matches `pandoc-engine.ts`'s cancellation cadence.
                const cts = new vscode.CancellationTokenSource();
                const cancelPoll = setInterval(() => {
                    if (externalController.cancelled) cts.cancel();
                }, 100);
                try {
                    const result = await deps.runKnit({
                        rBinary,
                        expression,
                        cwd,
                        timeoutMs,
                        output,
                        cancellation: cts.token,
                    });
                    outcome = classify(result, { cwd });
                } finally {
                    clearInterval(cancelPoll);
                    cts.dispose();
                }
            } else {
                outcome = await vscode.window.withProgress<KnitOutcome>(
                    {
                        location: vscode.ProgressLocation.Notification,
                        title: `Knitting ${baseName}…`,
                        cancellable: true,
                    },
                    async (_progress, token) => {
                        const result = await deps.runKnit({
                            rBinary,
                            expression,
                            cwd,
                            timeoutMs,
                            output,
                            cancellation: token,
                        });
                        return classify(result, { cwd });
                    },
                );
            }
        } finally {
            // Critical: registry.endOp runs the moment withProgress resolves,
            // BEFORE any user-facing toast is awaited. This is the Piece A
            // fix — under the previous code (a bare Set<string>), awaiting
            // showInformationMessage inside the withProgress callback held
            // both the progress notification AND the in-flight gate open
            // until the user dismissed the toast, causing a spurious
            // "already being knitted" on rapid re-invocation.
            //
            // Re-entry path: when this function was called with a null
            // registry (the export pipeline runs us under its own
            // `export-*` controller), there's no slot to release here —
            // the outer pipeline owns lifecycle.
            if (registry !== null && controller !== null) {
                registry.endOp(controller, controller.cancelled ? 'cancelled' : 'done');
            }
        }

        // Capture the outcome for callers (the export pipeline) that need
        // to distinguish a successful knit from a cancelled/failed one
        // that left a partial `.md`.
        if (outcomeCapture !== null) outcomeCapture.outcome = outcome;
        if (outcome.kind === 'ok') knitProducedValidMd = true;

        await renderOutcome(outcome, {
            fsPath,
            baseName,
            sourceUri: docUri,
            sourceLanguageId,
            cwd,
            knitRootDir,
            output,
            rBinary,
            timeoutMs,
            context,
            showOrUpdatePanel: deps.showOrUpdatePanel,
            getLanguageClient: deps.getLanguageClient,
            runPostKnitRender: deps.runPostKnitRender,
        });
    } finally {
        if (pinnedPreviewKey !== null && registry !== null) {
            // A successful knit just refreshed the dir contents, so
            // any deletion request triggered by the old panel's
            // disposal is stale — cancel it before unpinning so the
            // new panel created in renderOutcome can read the
            // `.html` we just wrote. On cancel/failure we leave the
            // deletion request in place so the next unpin discharges
            // it (the dir contents are not what the user wants).
            if (knitProducedValidMd) {
                registry.cancelPreviewDirDeletion(pinnedPreviewKey);
            }
            registry.unpinPreviewDir(pinnedPreviewKey);
        }
    }
}

interface RenderOutcomeCtx {
    fsPath: string;
    baseName: string;
    sourceUri: vscode.Uri;
    sourceLanguageId: string;
    cwd: string | undefined;
    /** Chunk working directory (`root.dir`); null = inherit subprocess cwd. */
    knitRootDir: string | null;
    output: vscode.OutputChannel;
    rBinary: string;
    timeoutMs: number;
    context: vscode.ExtensionContext;
    showOrUpdatePanel: KnitDeps['showOrUpdatePanel'];
    getLanguageClient: KnitDeps['getLanguageClient'];
    runPostKnitRender: KnitDeps['runPostKnitRender'];
}

/**
 * Surface the result of a knit to the user. Runs OUTSIDE the
 * `vscode.window.withProgress` callback so that the progress
 * notification closes the moment the R subprocess exits, regardless of
 * how long the user takes to dismiss the success/failure toast.
 */
async function renderOutcome(outcome: KnitOutcome, ctx: RenderOutcomeCtx): Promise<void> {
    if (outcome.kind === 'spawnError') {
        const code = outcome.error.code;
        if (code === 'ENOENT') {
            ctx.output.appendLine(`[error] R not found at "${ctx.rBinary}".`);
            void vscode.window.showErrorMessage(
                'Raven: Knit Preview — R not found on PATH. Set `raven.packages.rPath`.',
            );
        } else {
            ctx.output.appendLine(`[error] ${outcome.error.message}`);
            void vscode.window.showErrorMessage(
                `Raven: Knit Preview — failed to launch R: ${outcome.error.message}`,
            );
        }
        return;
    }

    if (outcome.kind === 'cancelled') {
        ctx.output.appendLine('Knit cancelled.');
        void vscode.window.showInformationMessage('Raven: Knit Preview cancelled.');
        return;
    }

    if (outcome.kind === 'timedOut') {
        ctx.output.appendLine(`Knit timed out after ${ctx.timeoutMs} ms.`);
        ctx.output.show(true);
        void vscode.window.showErrorMessage('Raven: Knit Preview timed out.');
        return;
    }

    if (outcome.kind === 'failed') {
        ctx.output.show(true);
        void vscode.window.showErrorMessage(
            `Raven: Knit Preview failed (exit ${outcome.exitCode}). See Raven: Knit output.`,
        );
        return;
    }

    if (outcome.kind === 'noOutput') {
        const SHOW = 'Show Output';
        const choice = await vscode.window.showInformationMessage(
            'Raven: Knit Preview succeeded (output path unknown).',
            SHOW,
        );
        if (choice === SHOW) ctx.output.show(true);
        return;
    }

    // ok branch — `knitr::knit` writes an intermediate `.md` and the
    // R expression emits one `Output created:` line pointing at it.
    // Our post-knit renderer turns that `.md` into the final `.html`
    // by running it through VS Code's markdown pipeline + Raven's
    // syntax highlighter. The panel shows the resulting `.html`;
    // "Open in Browser" opens the same `.html` so styling is
    // consistent across surfaces.
    const base = outcome.cwd ?? path.dirname(ctx.fsPath);
    const absolutized = outcome.parsedOutputs.map((p) => absolutizeFromCwd(p, base));
    const primary = pickPrimaryOutput(absolutized);
    if (!primary) {
        // Defensive — classify guarantees parsedOutputs.length >= 1 for 'ok'.
        void vscode.window.showInformationMessage('Raven: Knit Preview succeeded.');
        return;
    }

    const mdPath = primary;
    const htmlPath = computeHtmlOutputPath(ctx.fsPath);
    try {
        await ctx.runPostKnitRender({
            mdPath,
            htmlPath,
            context: ctx.context,
            client: ctx.getLanguageClient(),
            sourceUri: ctx.sourceUri,
            sourceLanguageId: ctx.sourceLanguageId,
            // The same `.html` is loaded by both the panel iframe
            // AND "Open in Browser", so the file can't carry
            // surface-specific theme logic — it's a frozen
            // snapshot. We leave `themeClasses` null so
            // `composeStylesheet` emits both palettes and swaps
            // them on `@media (prefers-color-scheme: dark)`:
            //
            //   - Browser: the media query resolves against the
            //     host OS, so the file auto-detects light/dark.
            //   - Webview iframe: VS Code reports
            //     `prefers-color-scheme` via the editor theme
            //     kind (which usually mirrors the OS — with
            //     `window.autoDetectColorScheme` on, it's exactly
            //     the OS).
            //
            // Users who want the panel to paint VS Code's editor
            // theme regardless of OS can toggle "Apply VS Code
            // theme" — that overlay supersedes the baked palette
            // and re-resolves on every theme change.
            themeClasses: null,
        });
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        ctx.output.appendLine(`[render] post-knit render failed: ${message}`);
        ctx.output.show(true);
        // Knit itself succeeded — `mdPath` exists and is readable.
        // Only the HTML render step failed. Offer the user a way
        // to still see the markdown so a render-step regression
        // (e.g. KaTeX CSS read failure, grammar registry init
        // error) doesn't strand a successful knit with "no output
        // at all" reported to the UI.
        const OPEN_MD = 'Open Markdown';
        const SHOW_OUTPUT = 'Show Output';
        const choice = await vscode.window.showErrorMessage(
            `Raven: Knit Preview produced ${path.basename(mdPath)} but the HTML render step failed. ` +
                `See Raven: Knit output for details.`,
            OPEN_MD,
            SHOW_OUTPUT,
        );
        if (choice === OPEN_MD) {
            try {
                const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(mdPath));
                await vscode.window.showTextDocument(doc, { preview: false });
            } catch (openErr) {
                const openMsg = openErr instanceof Error ? openErr.message : String(openErr);
                ctx.output.appendLine(`[render] failed to open ${mdPath}: ${openMsg}`);
                void vscode.window.showErrorMessage(
                    `Raven: Knit Preview — could not open ${path.basename(mdPath)}: ${openMsg}`,
                );
            }
        } else if (choice === SHOW_OUTPUT) {
            ctx.output.show(true);
        }
        return;
    }

    const panelResult = await ctx.showOrUpdatePanel(ctx.context, {
        sourceUri: ctx.sourceUri,
        outputPath: htmlPath,
        output: ctx.output,
        // Base for resolving relative `include_graphics` image paths.
        // Prefer the effective `root.dir` the subprocess REPORTED (it
        // reflects a runtime `opts_knit$set(root.dir=…)` override a
        // setup chunk may have applied); fall back to the statically
        // resolved value, then the subprocess cwd.
        knitRootDir: outcome.rootDir ?? ctx.knitRootDir ?? ctx.cwd,
    });
    if (!panelResult.ok) {
        ctx.output.appendLine(`[panel] ${panelResult.error}`);
        void revealKnitOutput(htmlPath);
        return;
    }
    // No success popover here: the panel itself is the success
    // signal, and a toast with a "Show Output Panel" button just
    // points at content that's already on screen. If knit produced
    // additional outputs (rare under the new pipeline since we only
    // run knitr::knit, which writes exactly one .md), log them.
    if (absolutized.length > 1) {
        ctx.output.appendLine(
            `[outputs] knit produced ${absolutized.length} files; HTML shown in panel:`,
        );
        for (const p of absolutized) {
            ctx.output.appendLine(`  - ${p}${p === primary ? ' (primary)' : ''}`);
        }
    }
}

/**
 * Test-only entry point that bypasses the registered `raven.knit`
 * command. Exposes the same code path with caller-controlled deps.
 * Used by `knit-progress-lifecycle.test.ts` to verify the Piece A
 * invariant: the registry slot is released the moment `withProgress`
 * resolves, NOT when the user dismisses the success toast.
 *
 * Accepts either a `Set<string>` (legacy — existing tests still pass
 * one in; it's adapted to an internal OperationRegistry) or a real
 * `OperationRegistry`. New tests should prefer the registry.
 *
 * The `__` prefix signals "test-only"; do not call from production
 * code.
 */
export async function __runKnitCommandForTest(args: {
    uri: vscode.Uri | undefined;
    output: vscode.OutputChannel;
    inFlight: Set<string> | OperationRegistry;
    context: vscode.ExtensionContext;
    deps: KnitDeps;
}): Promise<void> {
    const registry =
        args.inFlight instanceof OperationRegistry ? args.inFlight : new OperationRegistry();
    await runKnitCommand(args.uri, args.output, registry, args.context, args.deps);
}

interface KnitDirOk {
    ok: true;
    /** Value for `knitr::opts_knit$set(root.dir = …)` — the chunk working directory; null = inherit the subprocess CWD. */
    knitRootDir: string | null;
    /**
     * cwd for the R subprocess. `undefined` = inherit Node's
     * `process.cwd()` (the spec's "R's working directory at subprocess
     * start" — only used in `current` mode without a workspace).
     */
    cwd: string | undefined;
}
interface KnitDirErr { ok: false; error: string; }
type KnitDirResult = KnitDirOk | KnitDirErr;

/**
 * Map the `raven.knit.workingDirectory` mode to the pair (subprocess
 * cwd, `knit_root_dir` argument):
 *
 *   - `document` (default): subprocess cwd = `knit_root_dir` = the
 *     document's parent directory.
 *   - `project`: both = the workspace folder containing the document.
 *     Refuses if the document is outside every workspace folder.
 *   - `current`: omit `knit_root_dir`. When a workspace is open, use the
 *     first workspace folder as cwd (matches VS Code's convention that
 *     R-started-from-the-workspace inherits the workspace root). When
 *     no workspace is open, inherit Node's `process.cwd()` so we don't
 *     pretend the document directory is "R's startup wd" — the spec is
 *     specifically about not pinning a directory in this mode.
 */
function resolveKnitDir(
    docUri: vscode.Uri,
    mode: WorkingDirectoryMode,
): KnitDirResult {
    const fsPath = docUri.fsPath;
    if (mode === 'document') {
        const dir = path.dirname(fsPath);
        return { ok: true, knitRootDir: dir, cwd: dir };
    }
    if (mode === 'project') {
        const folder = vscode.workspace.getWorkspaceFolder(docUri);
        if (!folder) {
            return {
                ok: false,
                error: 'Raven: Knit Preview — cannot resolve project root: document is outside the workspace.',
            };
        }
        return { ok: true, knitRootDir: folder.uri.fsPath, cwd: folder.uri.fsPath };
    }
    // mode === 'current'
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    return {
        ok: true,
        knitRootDir: null,
        cwd: workspaceRoot,
    };
}

function resolveRBinary(): string {
    const configured = vscode.workspace
        .getConfiguration('raven.packages')
        .get<string>('rPath', '')
        .trim();
    return configured.length > 0 ? configured : 'R';
}

function readTimeoutMs(): number {
    const configured = vscode.workspace
        .getConfiguration('raven.knit')
        .get<number>('timeoutMs', DEFAULT_TIMEOUT_MS);
    if (typeof configured !== 'number' || !Number.isFinite(configured) || configured <= 0) {
        return DEFAULT_TIMEOUT_MS;
    }
    return configured;
}

function absolutizeFromCwd(raw: string, cwd: string): string {
    if (path.isAbsolute(raw)) return raw;
    return path.resolve(cwd, raw);
}


async function showBlocker(blocker: Blocker, fsPath: string): Promise<void> {
    const COPY = 'Copy command';
    // The blocker's copyCommand uses `'FILENAME'` as a quoted
    // placeholder. Substitute the actual path as a properly escaped R
    // literal so Windows backslashes and paths containing apostrophes
    // stay valid R syntax.
    // Replace every `'FILENAME'` placeholder. We use a string-splitting
    // join rather than `String.prototype.replaceAll` to keep
    // compatibility with the project's pre-ES2021 lib target. The
    // placeholder is single-quoted so `escapeRString` (also
    // single-quoted) is a drop-in substitution that stays valid R when
    // the path contains backslashes or apostrophes.
    const filledCommand = blocker.copyCommand
        .split("'FILENAME'")
        .join(escapeRString(fsPath));
    const choice = await vscode.window.showInformationMessage(
        blocker.message,
        { modal: false },
        COPY,
    );
    if (choice === COPY) {
        await vscode.env.clipboard.writeText(filledCommand);
        await vscode.window.showInformationMessage('Command copied to clipboard.');
    }
}

/**
 * Reveal non-HTML knit output. HTML outputs route through the Knit
 * Output webview panel instead (see `renderOutcome`). PDFs / Word docs
 * / etc. open via the OS file browser — the user double-clicks to
 * launch their preferred reader.
 */
async function revealKnitOutput(outputPath: string): Promise<void> {
    const uri = vscode.Uri.file(outputPath);
    await vscode.commands.executeCommand('revealFileInOS', uri);
}
