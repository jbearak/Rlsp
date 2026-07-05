/**
 * Pandoc-driven export commands.
 *
 *   - `raven.knit.exportHtml`
 *   - `raven.knit.exportPdf`
 *   - `raven.knit.exportDocx`
 *
 * Two entry-point modes:
 *
 *   - `editor-toolbar`: re-knit fresh into the preview temp dir,
 *     then run Pandoc and save next to the .Rmd. The user invokes
 *     these from the editor-title Raven menu.
 *   - `webview`: reuse the cached `.md` produced by the most recent
 *     Knit Preview (Approach C). The user invokes these via the
 *     `Export ▾` button in the webview's toolbar.
 *
 * Both modes pin the preview temp dir in the OperationRegistry while
 * export work references it so panel disposal can't yank the `.md` or
 * figure assets out from under us.
 *
 * In both modes the export pipeline:
 *
 *   1. Resolves Pandoc (lazy, on first use), surfaces an actionable
 *      error if missing.
 *   2. Parses YAML output options and builds Pandoc args.
 *   3. Runs `pandocConvert` which writes to a temp file and renames
 *      atomically over the destination — partial output never clobbers
 *      a prior good file.
 *   4. Shows a "Saved …" notification with an Open button via the
 *      shared `openExportedFile` helper.
 *
 * The whole thing runs inside `vscode.window.withProgress({ cancellable:
 * true })`. Cancelling sends SIGINT → SIGTERM → SIGKILL to the running
 * subprocess (R during knit, Pandoc during convert).
 */

import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import { extractFrontmatter, parseFrontmatter } from './yaml-frontmatter';
import { parseOutputOptions, type TargetFormat } from './output-options';
import { buildPandocArgs } from './pandoc-args';
import { pandocConvert } from './pandoc-engine';
import { PandocResolver, PandocNotFoundError } from './pandoc-detect';
import { OperationRegistry, type OpKind, type OperationController, type OpPhase } from './operation-controller';
import { canonicalOpKey, previewArtifactPaths } from './raven-knit-paths';
import { openExportedFile, type ExportFormat } from './open-exported-file';

export interface ExportDeps {
    resolver: PandocResolver;
    registry: OperationRegistry;
    getOutput: () => vscode.OutputChannel;
    /**
     * Invoked to run a fresh knit when the editor-toolbar entry needs
     * one. Receives the export's OperationController so the inner
     * subprocess can listen to the export's cancellation signal —
     * cancelling the export must stop the R subprocess mid-knit, not
     * let it run to completion. The result tells the export pipeline
     * whether the knit actually produced complete `.md` content; a
     * cancelled / failed / timed-out knit can leave a partial file
     * that would otherwise pass a bare `fs.existsSync` check.
     *
     * `targetFormat` tells the underlying knit which YAML `output:`
     * block to consult for chunk options. Editor-toolbar PDF/Word
     * exports pass 'pdf' / 'docx' so figures come from
     * `pdf_document` / `word_document` settings, not from
     * `html_document` defaults.
     *
     * Production wires this to `runKnitWithExistingController`; tests
     * override.
     */
    runKnit: (
        uri: vscode.Uri,
        exportController: OperationController,
        targetFormat: TargetFormat,
    ) => Promise<{ ok: boolean }>;
    /**
     * Push the webview Export button's busy state. Production wires this
     * to `KnitOutputPanel.notifyExportBusy`; tests can omit (or stub) it.
     * The export pipeline drives the value off the operation phase:
     * `true` while the op is in flight, `false` on `done` / `cancelled`.
     */
    notifyExportBusy?: (rmdAbsPath: string, busy: boolean) => void;
}

const EXPORT_OP_KIND: Record<TargetFormat, OpKind> = {
    html: 'export-html',
    pdf: 'export-pdf',
    docx: 'export-docx',
};

const EXPORT_EXTENSION: Record<TargetFormat, string> = {
    html: 'html',
    pdf: 'pdf',
    docx: 'docx',
};

export function registerExportCommands(context: vscode.ExtensionContext, deps: ExportDeps): void {
    const register = (id: string, format: TargetFormat): void => {
        context.subscriptions.push(
            // Second positional arg is an optional `{ entry }` hint passed by
            // the panel when the user clicks Export ▾ in the webview.
            // Default is `editor-toolbar`, which re-knits fresh; `webview`
            // reuses the cached preview .md without re-running R chunks.
            vscode.commands.registerCommand(id, async (uri?: vscode.Uri, opts?: Partial<RunExportOpts>) => {
                const target = uri ?? vscode.window.activeTextEditor?.document.uri;
                if (!target) {
                    void vscode.window.showWarningMessage('No R Markdown file selected to export.');
                    return;
                }
                const ext = path.extname(target.fsPath).toLowerCase();
                if (ext !== '.rmd' && ext !== '.rmarkdown') {
                    void vscode.window.showWarningMessage(`Cannot export ${path.basename(target.fsPath)} — not an R Markdown file.`);
                    return;
                }
                const entry: RunExportOpts['entry'] = opts?.entry ?? 'editor-toolbar';
                await runExport(target, format, deps, { entry });
            }),
        );
    };
    register('raven.knit.exportHtml', 'html');
    register('raven.knit.exportPdf', 'pdf');
    register('raven.knit.exportDocx', 'docx');

    // Cancel-export command. The webview's Export ▾ button dispatches
    // this when the user clicks it while an export is already in flight
    // (the button's busy state). The registry is the single source of
    // truth for "is there a running export"; we look up the controller
    // for the source URI and `cancel()` it. No-op when nothing is
    // running.
    context.subscriptions.push(
        vscode.commands.registerCommand('raven.knit.cancelExport', (uri?: vscode.Uri) => {
            const target = uri ?? vscode.window.activeTextEditor?.document.uri;
            if (!target) return;
            const op = deps.registry.current(canonicalOpKey(target));
            // Cancel any export-flavored op for this source: the three
            // plain `export-*` kinds plus the planned `knit-then-export`
            // (an editor-toolbar export that re-knits first as a single
            // cancellable op). Excludes `knit-preview` because the
            // Knit Preview panel has its own progress-notification
            // cancel path for that.
            if (op && op.kind !== 'knit-preview') op.cancel();
        }),
    );
}

export interface RunExportOpts {
    /** webview = reuse cached .md (Approach C); editor-toolbar = re-knit fresh. */
    entry: 'webview' | 'editor-toolbar';
}

/**
 * Exported for use by `knit-output-panel.ts` when the user picks an
 * export format from the webview's Export ▾ quickpick.
 */
export async function runExport(
    rmd: vscode.Uri,
    format: TargetFormat,
    deps: ExportDeps,
    opts: RunExportOpts,
): Promise<void> {
    const key = canonicalOpKey(rmd);
    const opKind = EXPORT_OP_KIND[format];
    const notifyBusy = deps.notifyExportBusy;
    const begin = deps.registry.beginOp(key, opKind, {
        broadcast: (phase: OpPhase) => {
            // Drive the webview Export button between its idle "open
            // quickpick" state and its busy "cancel in-flight export"
            // state. Terminal phases (done / cancelled) clear busy so
            // the next click opens the quickpick again. The notifier is
            // a no-op when no panel is open for this source.
            if (!notifyBusy) return;
            const busy = phase !== 'done' && phase !== 'cancelled';
            notifyBusy(rmd.fsPath, busy);
        },
    });
    if (begin.kind === 'busy') {
        await offerCancelAndRetryToast(
            begin.existing,
            rmd,
            () => runExport(rmd, format, deps, opts),
            deps.registry,
        );
        return;
    }
    const controller = begin.controller;

    let pinnedPreviewKey: string | null = null;
    // The "Saved …" toast (with Open / View) is shown OUTSIDE the
    // `withProgress` block — otherwise its `showInformationMessage`
    // would keep the "Exporting to …" progress popover open and the
    // Export button stuck on "Cancel export" until the user dismissed
    // the toast. `runExportInner` returns the destination it produced
    // (when Pandoc succeeded) and the caller fires the toast after
    // progress + busy state have cleared.
    let innerResult: ExportInnerResult | undefined;
    try {
        innerResult = await vscode.window.withProgress<ExportInnerResult>(
            {
                location: vscode.ProgressLocation.Notification,
                cancellable: true,
                title: `Exporting to ${format.toUpperCase()}…`,
            },
            async (_progress, token) => {
                token.onCancellationRequested(() => controller.cancel());
                return await runExportInner(rmd, format, deps, opts, controller, (key: string) => {
                    pinnedPreviewKey = key;
                });
            },
        );
    } finally {
        if (pinnedPreviewKey !== null) deps.registry.unpinPreviewDir(pinnedPreviewKey);
        deps.registry.endOp(controller, controller.cancelled ? 'cancelled' : 'done');
    }

    if (innerResult?.kind === 'success' && !controller.cancelled) {
        await openExportedFile(
            vscode.Uri.file(innerResult.destPath),
            innerResult.format,
            deps.getOutput(),
        );
    }
}

type ExportInnerResult =
    | { kind: 'success'; destPath: string; format: ExportFormat }
    | { kind: 'aborted' };

async function runExportInner(
    rmd: vscode.Uri,
    format: TargetFormat,
    deps: ExportDeps,
    opts: RunExportOpts,
    controller: OperationController,
    onPin: (previewKey: string) => void,
): Promise<ExportInnerResult> {
    const output = deps.getOutput();
    const previewPaths = previewArtifactPaths(rmd.fsPath);

    // [1] Ensure we have a .md to feed Pandoc.
    if (opts.entry === 'webview') {
        if (!fs.existsSync(previewPaths.mdPath)) {
            void vscode.window.showWarningMessage('No cached preview. Knit first, then export.');
            return { kind: 'aborted' };
        }
    }

    deps.registry.pinPreviewDir(previewPaths.previewKey);
    onPin(previewPaths.previewKey);

    if (opts.entry === 'editor-toolbar') {
        // editor-toolbar: re-knit fresh.
        //
        // Critical: `runKnit` (= `vscode.commands.executeCommand('raven.knit', uri)`)
        // does NOT throw on knit failure — the command handler in
        // `knit-commands.ts` surfaces errors via `vscode.window.showErrorMessage`
        // and returns. Without the pre-delete below, a stale `.md`
        // left over from a previous successful knit would silently
        // satisfy the existence check and we'd export an outdated
        // document. Delete first, then knit, then verify the .md was
        // re-created. If knit didn't run / aborted / failed the
        // existence check now correctly fails the export.
        try {
            await fs.promises.unlink(previewPaths.mdPath);
        } catch (err) {
            // ENOENT is fine — there was nothing to delete. Anything
            // else (EACCES, etc.) is fatal: continuing would risk
            // exporting whichever file happened to be there.
            if ((err as NodeJS.ErrnoException).code !== 'ENOENT') {
                output.appendLine(
                    `[Export] could not remove stale preview .md at ${previewPaths.mdPath}: ${(err as Error).message}`,
                );
                return { kind: 'aborted' };
            }
        }
        controller.updatePhase('knitting');
        let knitResult: { ok: boolean };
        try {
            // Pass the export target so the underlying knit picks
            // chunk options from `pdf_document` / `word_document` /
            // `html_document` to match — otherwise a PDF export
            // would knit with HTML's defaults and Pandoc would
            // convert HTML-sized figures into the wrong target.
            knitResult = await deps.runKnit(rmd, controller, format);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            output.appendLine(`[Export] knit failed: ${msg}`);
            return { kind: 'aborted' };
        }
        if (!knitResult.ok) {
            // Cancelled, timed out, spawn-errored, or otherwise non-ok.
            // The `.md` on disk may be a partial write left over from the
            // failed run; either way we cannot safely export it. The
            // user has already seen the knit failure UI via renderOutcome.
            output.appendLine('[Export] aborting because the underlying knit did not succeed.');
            return { kind: 'aborted' };
        }
        if (!fs.existsSync(previewPaths.mdPath)) {
            // Defensive — knitResult.ok should imply the file exists,
            // but if knitr ever changes its contract this is the
            // backstop that prevents Pandoc from being handed a
            // non-existent input path.
            output.appendLine(`[Export] knit reported success but no .md at ${previewPaths.mdPath}; aborting.`);
            return { kind: 'aborted' };
        }
        // A disposal of the previous panel during the export pin may
        // have queued deletion. The successful re-knit made this dir
        // live again, so the final unpin must not remove it.
        deps.registry.cancelPreviewDirDeletion(previewPaths.previewKey);
    }

    // [2] Resolve Pandoc.
    controller.updatePhase('converting');
    let pandocBin: string;
    try {
        pandocBin = await deps.resolver.resolve();
    } catch (err) {
        if (err instanceof PandocNotFoundError) {
            await offerPandocInstall();
            return { kind: 'aborted' };
        }
        throw err;
    }

    // [3] Parse YAML + build Pandoc args.
    let documentText: string;
    try {
        // `vscode.workspace.fs.readFile` is typed as `Uint8Array`. A bare
        // `.toString()` on a true `Uint8Array` returns a comma-separated
        // byte list, which would make `extractFrontmatter` miss the YAML
        // fence even on perfectly-formed documents. Decode as UTF-8.
        documentText = Buffer.from(await vscode.workspace.fs.readFile(rmd)).toString('utf8');
    } catch (err) {
        output.appendLine(`[Export] failed to read source: ${err instanceof Error ? err.message : String(err)}`);
        return { kind: 'aborted' };
    }
    const fmInner = extractFrontmatter(documentText) ?? '';
    const fmParse = parseFrontmatter(fmInner);
    const fm = fmParse.ok ? fmParse.value : {};
    const outOpts = parseOutputOptions(fm, format);

    for (const key of outOpts.ignored) {
        output.appendLine(`[knit] Ignored output: option '${key}'`);
    }
    if (outOpts.droppedPandocArgs.length > 0) {
        const list = outOpts.droppedPandocArgs.map((a) => JSON.stringify(a)).join(' ');
        output.appendLine(
            `[knit] Stripped destination/format args from pandoc_args (menu choice wins): ${list}`,
        );
    }

    const sourceDir = path.dirname(rmd.fsPath);
    const workspaceFolder = vscode.workspace.getWorkspaceFolder(rmd)?.uri.fsPath;
    const containmentRoot = workspaceFolder ?? sourceDir;
    const baseName = path
        .basename(rmd.fsPath)
        .replace(/\.(?:[Rr][Mm][Dd]|[Rr][Mm][Aa][Rr][Kk][Dd][Oo][Ww][Nn])$/, '');
    const destPath = path.join(sourceDir, `${baseName}.${EXPORT_EXTENSION[format]}`);

    const pdfEngine = resolvePdfEngineSetting(
        vscode.workspace.getConfiguration('raven').get<string>('pandoc.pdfEngine', 'xelatex'),
        output,
    );
    const detailed = buildPandocArgs.detailed(outOpts, format, {
        mdPath: previewPaths.mdPath,
        outPath: destPath,
        sourceDir,
        containmentRoot,
        pdfEngine,
    });
    for (const dropped of detailed.droppedCss) {
        output.appendLine(`[knit] CSS path outside containment root, dropped: '${dropped}'`);
    }
    for (const flag of detailed.ignoredFlags) {
        output.appendLine(`[knit] Ignored output: ${flag}`);
    }

    // [4] Run Pandoc.
    const timeoutMs = vscode.workspace.getConfiguration('raven').get<number>('knit.export.timeoutMs', 120_000);
    const result = await pandocConvert({
        pandocPath: pandocBin,
        args: detailed.args,
        mdPath: previewPaths.mdPath,
        destPath,
        cwd: previewPaths.previewDir,
        timeoutMs,
        controller,
        output,
    });

    controller.updatePhase('finalizing');
    if (result.status === 'success') {
        // Defer the "Saved …" toast to the caller — see the comment in
        // `runExport`. Showing it here would keep the progress popover
        // and Export-busy state alive until the user clicked Open/View.
        return { kind: 'success', destPath, format: formatToExport(format) };
    } else if (result.status === 'cancelled') {
        output.appendLine('[Export] Cancelled.');
        return { kind: 'aborted' };
    } else {
        await offerPandocFailure(format, result.stderr, output, rmd);
        return { kind: 'aborted' };
    }
}

function formatToExport(f: TargetFormat): ExportFormat {
    return f;
}

/**
 * Allowlist of Pandoc PDF engines that match the `package.json` enum.
 * `getConfiguration().get<string>(...)` returns whatever the user wrote
 * in `settings.json`, bypassing the JSON-schema enum check, so we
 * re-validate here before handing the value to Pandoc as a flag. An
 * untrusted workspace could otherwise steer export at an attacker-
 * controlled binary via `--pdf-engine=<bogus>`.
 */
const PDF_ENGINE_ALLOWLIST: ReadonlySet<string> = new Set([
    'xelatex',
    'pdflatex',
    'lualatex',
    'tectonic',
    'wkhtmltopdf',
]);

function resolvePdfEngineSetting(raw: string, output: vscode.OutputChannel): string {
    if (PDF_ENGINE_ALLOWLIST.has(raw)) return raw;
    output.appendLine(
        `[Export] Ignored raven.pandoc.pdfEngine = ${JSON.stringify(raw)} (not in allowlist); falling back to xelatex.`,
    );
    return 'xelatex';
}

async function offerPandocInstall(): Promise<void> {
    const INSTALL = 'Install Pandoc…';
    const SET_PATH = 'Set path…';
    const choice = await vscode.window.showErrorMessage(
        'Pandoc not found. Install it to export to PDF, Word, or HTML.',
        INSTALL,
        SET_PATH,
    );
    if (choice === INSTALL) {
        void vscode.env.openExternal(vscode.Uri.parse('https://pandoc.org/installing.html'));
    } else if (choice === SET_PATH) {
        void vscode.commands.executeCommand('workbench.action.openSettings', '@id:raven.pandoc.path');
    }
}

async function offerPandocFailure(
    format: TargetFormat,
    stderr: string,
    output: vscode.OutputChannel,
    rmd: vscode.Uri,
): Promise<void> {
    output.appendLine(`[Export] Pandoc stderr:\n${stderr}`);

    // PDF-specific LaTeX engine hint. The `tectonic` regex is forgiving
    // about Pandoc's message phrasing across versions.
    if (format === 'pdf' && /(xelatex|pdflatex|lualatex|tectonic)[\s.,'"]*(?:not[\s-]?found|could[\s-]?not|is[\s-]?missing|no[\s-]?such)/i.test(stderr)) {
        const INSTALL = 'Install TinyTeX…';
        const SHOW = 'Show details';
        const choice = await vscode.window.showErrorMessage(
            'PDF export needs a LaTeX engine.',
            INSTALL,
            SHOW,
        );
        if (choice === INSTALL) {
            void vscode.env.openExternal(vscode.Uri.parse('https://yihui.org/tinytex/'));
        } else if (choice === SHOW) {
            output.show(true);
        }
        return;
    }

    const SHOW = 'Show details';
    const TRY_WORD = 'Try Word instead';
    const buttons = format === 'pdf' ? [SHOW, TRY_WORD] : [SHOW];
    const choice = await vscode.window.showErrorMessage(
        `Export to ${format.toUpperCase()} failed.`,
        ...buttons,
    );
    if (choice === SHOW) output.show(true);
    else if (choice === TRY_WORD) {
        void vscode.commands.executeCommand('raven.knit.exportDocx', rmd);
    }
}

async function offerCancelAndRetryToast(
    existing: OperationController,
    uri: vscode.Uri,
    retry: () => Promise<void>,
    registry?: OperationRegistry,
): Promise<void> {
    const CANCEL = 'Cancel and retry';
    const WAIT = 'Wait';
    const kind = humanizeOpKind(existing.kind);
    const choice = await vscode.window.showInformationMessage(
        `A ${kind} is in progress for ${path.basename(uri.fsPath)}.`,
        CANCEL,
        WAIT,
    );
    if (choice !== CANCEL) return;

    existing.cancel();
    // Wait up to ~5s for the in-flight op to settle; then proceed.
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline) {
        if (existing.phase === 'cancelled' || existing.phase === 'done') break;
        await new Promise((r) => setTimeout(r, 100));
    }
    // Recursion guard: if the prior controller is STILL the active op
    // in the registry after the deadline (slow subprocess shutdown,
    // hung file handle, AV scan), retrying would re-enter beginOp →
    // busy → this same function in an unbounded loop. Surface a
    // terminal toast instead and let the user decide.
    if (registry !== undefined && registry.current(existing.key) === existing) {
        const SHOW = 'Show details';
        const choice2 = await vscode.window.showWarningMessage(
            `Could not cancel the in-flight ${kind}. Wait for it to finish or restart the window.`,
            SHOW,
        );
        if (choice2 === SHOW) {
            void vscode.commands.executeCommand('raven.knit.openOutputChannel');
        }
        return;
    }
    await retry();
}

function humanizeOpKind(kind: OpKind): string {
    switch (kind) {
        case 'knit-preview':
            return 'knit';
        case 'export-html':
            return 'HTML export';
        case 'export-pdf':
            return 'PDF export';
        case 'export-docx':
            return 'Word export';
        case 'knit-then-export':
            return 'knit-then-export';
    }
}
