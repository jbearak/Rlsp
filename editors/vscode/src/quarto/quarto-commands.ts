/**
 * Quarto Preview / Render / Stop command policy.
 *
 * Preview and Render share an ordered preflight: URI selection, on-disk `file`
 * scheme and case-insensitive `.qmd` validation, workspace trust, open/save,
 * per-document Shiny rejection, then project-context discovery. Stop
 * intentionally bypasses every one of those gates: it performs no trust check,
 * save, frontmatter parse, or CLI resolution, and looks up the existing runtime
 * by current key or source alias. Preview registers a source-keyed pending
 * intent before its first await; Stop can cancel it during preflight or binary
 * discovery, and the continuation consumes it immediately before runtime
 * session claim.
 *
 * Render key ownership is reserved before the document is opened or saved and
 * covers preflight + resolver + subprocess work. Project renders share a
 * symlink-resolved project key; standalone renders use a symlink-resolved
 * source key when possible. Runtime ownership keeps its existing lexical
 * canonicalization.
 * The guard is released before install or outcome notifications are awaited,
 * so a completed render's toast cannot block the next invocation.
 * Activation-scoped command ownership lets deactivation await asynchronous
 * continuations under a bound. Notifications remain outside `withProgress`;
 * raw results use knit's precedence exactly: spawn error, then cancellation,
 * then timeout, so a cancellation racing the timer stays silent.
 */

import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import type { KnitEngineResult } from '../knit/knit-engine';
import { openExportedFile, type ExportFormat } from '../knit/open-exported-file';
import { parseRenderedOutputPath } from '../knit/output-path';
import { canonicalOpKey } from '../knit/raven-knit-paths';
import { extractFrontmatter, parseFrontmatter } from '../knit/yaml-frontmatter';
import { QuartoNotFoundError, type QuartoResolver } from './quarto-detect';
import { QuartoCommandLifecycle } from './quarto-command-lifecycle';
import { isShinyServerDocument } from './quarto-frontmatter';
import { stripAnsi } from './preview-url-parser';
import {
    resolveQuartoContext,
    type QuartoContext,
} from './quarto-project';
import { isQuartoProjectMarkerFile } from './quarto-project-fs';
import type {
    QuartoPendingStart,
    QuartoRuntime,
} from './quarto-preview-runtime';
import {
    classifyQuartoRenderResult,
    DEFAULT_QUARTO_RENDER_TIMEOUT_MS,
    normalizeQuartoRenderTimeoutMs,
    type QuartoRenderOptions,
} from './quarto-render-engine';
import { resolveQuartoRenderedOutputPath } from './quarto-render-output';

export interface QuartoCommandDeps {
    resolver: Pick<QuartoResolver<vscode.Uri>, 'resolve'>;
    runtime: Pick<
        QuartoRuntime,
        | 'startOrRestart'
        | 'stopByLookup'
        | 'registerPendingStart'
        | 'reconcilePendingStart'
        | 'consumePendingStart'
        | 'releasePendingStart'
    >;
    output: vscode.OutputChannel;
    runRender: (opts: QuartoRenderOptions) => Promise<KnitEngineResult>;
    resolveContext?: (sourceFsPath: string) => QuartoContext;
    isWorkspaceTrusted?: () => boolean;
    openTextDocument?: (uri: vscode.Uri) => Thenable<vscode.TextDocument>;
    /** Dependency-injected realpath seam for render-guard tests. */
    realpath?: (sourceFsPath: string) => string;
}

export interface QuartoPreflight {
    uri: vscode.Uri;
    document: vscode.TextDocument;
    context: QuartoContext;
}

type QuartoRenderRunResult =
    | {
        kind: 'completed';
        result: KnitEngineResult;
        timeoutMs: number;
        preflight: QuartoPreflight;
    }
    | { kind: 'preflight-stopped' }
    | { kind: 'quarto-not-found'; error: QuartoNotFoundError };

type QuartoRenderRunner = (uri?: vscode.Uri) => Promise<void>;

/** Register the four user-facing Quarto commands. */
export function registerQuartoCommands(
    context: vscode.ExtensionContext,
    deps: QuartoCommandDeps,
): QuartoCommandLifecycle {
    const commandLifecycle = new QuartoCommandLifecycle();
    const runGuardedRender = createQuartoRenderRunner(deps);

    context.subscriptions.push(
        vscode.commands.registerCommand('raven.quarto.preview', (uri?: vscode.Uri) =>
            commandLifecycle.run(() => runPreviewCommand(uri, deps))),
        vscode.commands.registerCommand('raven.quarto.render', (uri?: vscode.Uri) =>
            commandLifecycle.run(() => runGuardedRender(uri))),
        vscode.commands.registerCommand('raven.quarto.stopPreview', async (uri?: vscode.Uri) => {
            await runStopCommand(uri, deps);
        }),
        vscode.commands.registerCommand('raven.quarto.openOutputChannel', () => {
            deps.output.show(true);
        }),
    );
    return commandLifecycle;
}

/** Exported seam for Mocha command-policy tests. */
export async function runQuartoPreflightForTesting(
    uri: vscode.Uri | undefined,
    label: 'Preview' | 'Render',
    deps: QuartoCommandDeps,
): Promise<QuartoPreflight | null> {
    return runPreflight(uri, label, deps);
}

/** Exported seam proving Stop never enters Preview/Render preflight. */
export async function runQuartoStopForTesting(
    uri: vscode.Uri | undefined,
    deps: QuartoCommandDeps,
): Promise<void> {
    await runStopCommand(uri, deps);
}

/** Exported factory for render-lock and notification-ordering tests. */
export function createQuartoRenderRunnerForTesting(
    deps: QuartoCommandDeps,
): QuartoRenderRunner {
    return createQuartoRenderRunner(deps);
}

/** Exported factory for pending-Preview command-policy tests. */
export function createQuartoPreviewRunnerForTesting(
    deps: QuartoCommandDeps,
): (uri?: vscode.Uri) => Promise<void> {
    return (uri) => runPreviewCommand(uri, deps);
}

async function runPreviewCommand(
    explicitUri: vscode.Uri | undefined,
    deps: QuartoCommandDeps,
): Promise<void> {
    const uri = explicitUri ?? vscode.window.activeTextEditor?.document.uri;
    let pending: QuartoPendingStart | null = isFileQmdUri(uri)
        ? deps.runtime.registerPendingStart(uri.fsPath)
        : null;
    try {
        const preflight = await runPreflight(uri, 'Preview', deps);
        if (!preflight || !pending) return;
        if (!deps.runtime.reconcilePendingStart(pending, preflight.context.key)) return;

        let quartoPath: string;
        try {
            quartoPath = await deps.resolver.resolve(preflight.uri);
        } catch (err) {
            if (!deps.runtime.consumePendingStart(pending)) {
                pending = null;
                return;
            }
            pending = null;
            if (err instanceof QuartoNotFoundError) {
                await offerQuartoInstall(err);
                return;
            }
            throw err;
        }
        if (!deps.runtime.consumePendingStart(pending)) {
            pending = null;
            return;
        }
        pending = null;

        safeAppendLine(deps.output, `\n[preview] ${preflight.uri.fsPath}`);
        try {
            await deps.runtime.startOrRestart({
                key: preflight.context.key,
                cwd: preflight.context.cwd,
                sourceFsPath: preflight.uri.fsPath,
                quartoPath,
            });
        } catch (err) {
            if (isRuntimeDeactivatingError(err)) return;
            throw err;
        }
    } finally {
        if (pending) deps.runtime.releasePendingStart(pending);
    }
}

async function runPreflight(
    explicitUri: vscode.Uri | undefined,
    label: 'Preview' | 'Render',
    deps: QuartoCommandDeps,
): Promise<QuartoPreflight | null> {
    const uri = explicitUri ?? vscode.window.activeTextEditor?.document.uri;
    if (!uri) {
        await vscode.window.showInformationMessage(
            `Raven: Quarto ${label} requires an active .qmd document.`,
        );
        return null;
    }
    if (!await validateQuartoRunUri(uri, label)) return null;
    return runPreflightForValidatedUri(uri, label, deps);
}

async function validateQuartoRunUri(
    uri: vscode.Uri,
    label: 'Preview' | 'Render',
): Promise<boolean> {
    if (uri.scheme !== 'file') {
        await vscode.window.showInformationMessage(
            `Raven: Quarto ${label} needs a saved .qmd file on disk; ` +
            `this editor (${uri.scheme || 'unknown'}) isn't a file.`,
        );
        return false;
    }
    if (path.extname(uri.fsPath || uri.path).toLowerCase() !== '.qmd') {
        await vscode.window.showInformationMessage(
            `Raven: Quarto ${label} only runs on .qmd files.`,
        );
        return false;
    }
    return true;
}

function isFileQmdUri(uri: vscode.Uri | undefined): uri is vscode.Uri {
    return uri?.scheme === 'file'
        && path.extname(uri.fsPath || uri.path).toLowerCase() === '.qmd';
}

async function runPreflightForValidatedUri(
    uri: vscode.Uri,
    label: 'Preview' | 'Render',
    deps: QuartoCommandDeps,
    knownContext?: QuartoContext,
): Promise<QuartoPreflight | null> {
    const trusted = deps.isWorkspaceTrusted?.() ?? vscode.workspace.isTrusted;
    if (!trusted) {
        const manage = 'Manage Workspace Trust';
        const choice = await vscode.window.showInformationMessage(
            `Raven: Quarto ${label} is disabled in untrusted workspaces.`,
            manage,
        );
        if (choice === manage) {
            await vscode.commands.executeCommand('workbench.trust.manage');
        }
        return null;
    }

    let document: vscode.TextDocument;
    try {
        document = await (deps.openTextDocument?.(uri)
            ?? vscode.workspace.openTextDocument(uri));
    } catch (err) {
        await vscode.window.showErrorMessage(
            `Raven: Quarto ${label} could not open the document: ${errorMessage(err)}`,
        );
        return null;
    }
    if (document.isDirty) {
        let saved = false;
        try {
            saved = await document.save();
        } catch (err) {
            safeAppendLine(deps.output, `[quarto] save failed: ${errorMessage(err)}`);
        }
        if (!saved) {
            await vscode.window.showWarningMessage(
                `Raven: Quarto ${label} could not save ${path.basename(uri.fsPath)}. ` +
                'Quarto would not see the unsaved changes.',
            );
            return null;
        }
    }

    const frontmatter = extractFrontmatter(document.getText()) ?? '';
    const parsed = parseFrontmatter(frontmatter);
    if (parsed.ok && isShinyServerDocument(parsed.value)) {
        await vscode.window.showInformationMessage(
            'Raven: Quarto Preview and Render do not support `server: shiny`. ' +
            'Shiny documents require the separate `quarto serve` lifecycle.',
        );
        return null;
    }
    if (!parsed.ok) {
        safeAppendLine(
            deps.output,
            `[quarto] frontmatter parse failed; Quarto will validate it: ${parsed.error}`,
        );
    }

    return {
        uri,
        document,
        context: knownContext ?? resolveContextForSource(uri.fsPath, deps),
    };
}

async function runStopCommand(
    explicitUri: vscode.Uri | undefined,
    deps: QuartoCommandDeps,
): Promise<void> {
    const uri = explicitUri ?? vscode.window.activeTextEditor?.document.uri;
    if (!uri) {
        await vscode.window.showInformationMessage(
            'No Quarto preview is running for this document.',
        );
        return;
    }
    const resolveContext = deps.resolveContext ?? ((sourceFsPath: string) =>
        resolveQuartoContext(sourceFsPath, {
            isProjectMarkerFile: isQuartoProjectMarkerFile,
        }));
    const quartoContext = resolveContext(uri.fsPath);
    const outcome = await deps.runtime.stopByLookup(quartoContext.key, uri.fsPath);
    if (outcome === 'stopped') {
        await vscode.window.showInformationMessage('Quarto preview stopped.');
    } else if (outcome === 'none') {
        await vscode.window.showInformationMessage(
            'No Quarto preview is running for this document.',
        );
    }
    // already-stopping is intentionally a silent no-op.
}

function createQuartoRenderRunner(deps: QuartoCommandDeps): QuartoRenderRunner {
    const inFlightRenders = new Map<string, Promise<QuartoRenderRunResult>>();
    return async (explicitUri) => {
        const uri = explicitUri ?? vscode.window.activeTextEditor?.document.uri;
        if (!uri) {
            await runPreflight(undefined, 'Render', deps);
            return;
        }
        if (!await validateQuartoRunUri(uri, 'Render')) return;

        const context = resolveContextForSource(uri.fsPath, deps);
        const opKey = renderGuardKey(uri, context, deps);
        if (inFlightRenders.has(opKey)) {
            await vscode.window.showInformationMessage(
                `A Quarto render is already running for ` +
                `${path.basename(uri.fsPath)}.`,
            );
            return;
        }

        // Deferring the work by one microtask lets ownership enter the map
        // before openTextDocument/save can run, even for simultaneous calls.
        const run = Promise.resolve().then(async (): Promise<QuartoRenderRunResult> => {
            const preflight = await runPreflightForValidatedUri(
                uri,
                'Render',
                deps,
                context,
            );
            if (!preflight) return { kind: 'preflight-stopped' };
            return runRenderProcess(preflight, deps);
        });
        inFlightRenders.set(opKey, run);
        let completed: QuartoRenderRunResult;
        try {
            completed = await run;
        } finally {
            if (inFlightRenders.get(opKey) === run) inFlightRenders.delete(opKey);
        }

        if (completed.kind === 'preflight-stopped') return;
        if (completed.kind === 'quarto-not-found') {
            await offerQuartoInstall(completed.error);
            return;
        }
        await renderOutcome(
            completed.result,
            completed.preflight,
            completed.timeoutMs,
            deps.output,
        );
    };
}

function renderGuardKey(
    uri: vscode.Uri,
    context: QuartoContext,
    deps: QuartoCommandDeps,
): string {
    const target = context.projectRoot ?? uri.fsPath;
    const kind = context.projectRoot === null ? 'file' : 'project';
    try {
        const realpath = deps.realpath ?? fs.realpathSync.native;
        return `${kind}:${canonicalOpKey({ fsPath: realpath(target) })}`;
    } catch {
        const lexical = context.projectRoot === null
            ? canonicalOpKey(uri)
            : canonicalOpKey({ fsPath: context.key });
        return `${kind}:${lexical}`;
    }
}

function resolveContextForSource(
    sourceFsPath: string,
    deps: QuartoCommandDeps,
): QuartoContext {
    const resolveContext = deps.resolveContext ?? ((candidate: string) =>
        resolveQuartoContext(candidate, {
            isProjectMarkerFile: isQuartoProjectMarkerFile,
        }));
    return resolveContext(sourceFsPath);
}

async function runRenderProcess(
    preflight: QuartoPreflight,
    deps: QuartoCommandDeps,
): Promise<QuartoRenderRunResult> {
    let quartoPath: string;
    try {
        quartoPath = await deps.resolver.resolve(preflight.uri);
    } catch (err) {
        if (err instanceof QuartoNotFoundError) {
            return { kind: 'quarto-not-found', error: err };
        }
        throw err;
    }

    const configuredTimeoutMs = vscode.workspace
        .getConfiguration('raven.quarto', preflight.uri)
        .get<unknown>('render.timeoutMs', DEFAULT_QUARTO_RENDER_TIMEOUT_MS);
    const timeoutMs = normalizeQuartoRenderTimeoutMs(configuredTimeoutMs);
    safeAppendLine(deps.output, `\n[render] ${preflight.uri.fsPath}`);
    const result = await vscode.window.withProgress<KnitEngineResult>(
        {
            location: vscode.ProgressLocation.Notification,
            cancellable: true,
            title: `Rendering ${path.basename(preflight.uri.fsPath)} with Quarto…`,
        },
        async (_progress, cancellation) => deps.runRender({
            quartoPath,
            sourceFsPath: preflight.uri.fsPath,
            cwd: preflight.context.cwd,
            timeoutMs,
            output: deps.output,
            cancellation,
        }),
    );
    return { kind: 'completed', result, timeoutMs, preflight };
}

async function renderOutcome(
    result: KnitEngineResult,
    preflight: QuartoPreflight,
    timeoutMs: number,
    output: vscode.OutputChannel,
): Promise<void> {
    const kind = classifyQuartoRenderResult(result);
    if (kind === 'spawnError') {
        safeAppendLine(
            output,
            `[render] spawn error: ${result.spawnError?.message ?? 'unknown error'}`,
        );
        await offerQuartoInstall();
        return;
    }
    if (kind === 'cancelled') return;
    if (kind === 'timedOut') {
        const show = 'Show Output';
        const choice = await vscode.window.showErrorMessage(
            `Quarto render timed out after ${timeoutMs}ms (` +
            '`raven.quarto.render.timeoutMs`).',
            show,
        );
        if (choice === show) output.show(true);
        return;
    }
    if (kind === 'failed') {
        const show = 'Show Output';
        const choice = await vscode.window.showErrorMessage(
            `Quarto render failed (exit code ${String(result.exitCode)}).`,
            show,
        );
        if (choice === show) output.show(true);
        return;
    }

    const parsed = parseRenderedOutputPath(stripAnsi(`${result.stdout}\n${result.stderr}`));
    const last = parsed.paths.at(-1);
    if (!last) {
        await vscode.window.showInformationMessage(
            'Quarto render succeeded (exit 0); see the Raven: Quarto output channel.',
        );
        return;
    }
    const outputPath = resolveQuartoRenderedOutputPath(
        last,
        preflight.uri.fsPath,
        preflight.context.cwd,
    );
    const uri = vscode.Uri.file(outputPath);
    const format = exportedFormat(outputPath);
    if (format) {
        await openExportedFile(uri, format, output, 'Raven: Quarto');
        return;
    }
    const reveal = 'Reveal';
    const choice = await vscode.window.showInformationMessage(
        `Rendered ${path.basename(outputPath)}`,
        reveal,
    );
    if (choice === reveal) {
        await vscode.commands.executeCommand('revealInExplorer', uri);
    }
}

function exportedFormat(outputPath: string): ExportFormat | null {
    const ext = path.extname(outputPath).toLowerCase();
    if (ext === '.html' || ext === '.htm') return 'html';
    if (ext === '.pdf') return 'pdf';
    if (ext === '.docx') return 'docx';
    return null;
}

async function offerQuartoInstall(error?: QuartoNotFoundError): Promise<void> {
    const install = 'Install…';
    const setPath = 'Set Path…';
    const message = error?.message.startsWith('Configured Quarto path')
        ? error.message
        : 'Quarto CLI not found. Install Quarto or configure `raven.quarto.path`.';
    const choice = await vscode.window.showErrorMessage(
        message,
        install,
        setPath,
    );
    if (choice === install) {
        await vscode.env.openExternal(
            vscode.Uri.parse('https://quarto.org/docs/get-started/'),
        );
    } else if (choice === setPath) {
        await vscode.commands.executeCommand(
            'workbench.action.openSettings',
            '@id:raven.quarto.path',
        );
    }
}

function errorMessage(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}

function isRuntimeDeactivatingError(err: unknown): boolean {
    return err instanceof Error
        && err.message.includes('Quarto runtime is deactivating');
}

function safeAppendLine(output: vscode.OutputChannel, value: string): void {
    try { output.appendLine(value); } catch { /* output may be disposed */ }
}
