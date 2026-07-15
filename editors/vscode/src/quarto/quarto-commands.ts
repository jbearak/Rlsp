/**
 * Quarto Preview / Render / Stop command policy.
 *
 * Preview and Render share URI, trust, save, and frontmatter policy. Render
 * performs its non-mutating async project-context lookup before open/save so
 * it can reserve the physical guard key before those side effects; Preview
 * discovers context within preflight. Stop intentionally bypasses the mutating
 * gates and CLI resolution, first looking up the lexical source alias before
 * any async project fallback. Preview registers a source-keyed pending
 * intent before its first await; Stop can cancel it during preflight or binary
 * discovery, and the continuation consumes it immediately before runtime
 * session claim.
 *
 * Render key ownership is reserved before the document is opened or saved and
 * covers preflight + resolver + subprocess work. Async context discovery
 * realpaths the source before classifying it, so project and standalone guard
 * keys share one physical identity even when the editor URI is a symlink.
 * Runtime source-alias ownership keeps its existing lexical canonicalization.
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
import { cancelableDelay } from './quarto-cancelable-delay';
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
    QuartoStopResult,
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
    >
        // Optional so command-policy test fakes need not stub them; the
        // production runtime always provides both.
        & Partial<Pick<QuartoRuntime, 'beginStop' | 'hasPendingStarts'>>;
    output: vscode.OutputChannel;
    runRender: (opts: QuartoRenderOptions) => Promise<KnitEngineResult>;
    resolveContext?: (sourceFsPath: string) => Promise<QuartoContext>;
    isWorkspaceTrusted?: () => boolean;
    openTextDocument?: (uri: vscode.Uri) => Thenable<vscode.TextDocument>;
    /** Dependency-injected clock for render-output freshness tests. */
    now?: () => number;
    /** Project-discovery timeout override; tests use a tiny bound. */
    contextTimeoutMs?: number;
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
        renderStartMs: number;
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
        vscode.commands.registerCommand('raven.quarto.stopPreview', (uri?: vscode.Uri) =>
            commandLifecycle.run(() => runStopCommand(uri, deps))),
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
        context: knownContext ?? await resolveContextForSource(uri.fsPath, deps),
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
    // Take the fast lexical/source-alias path first and avoid remote-fs project
    // discovery on the common case. Allocate one stop epoch synchronously,
    // before any await, and share it across both phases: this stamps the Stop at
    // its issue time so a Preview for a sibling file launched *after* this click
    // is not abandoned by the (later) project-key phase's epoch record.
    const stopEpoch = deps.runtime.beginStop?.();
    const lexicalKey = canonicalOpKey(uri);
    let outcome = await deps.runtime.stopByLookup(lexicalKey, uri.fsPath, stopEpoch);
    const stoppedSession = outcome === 'stopped';
    // Whether the fast lexical phase already found something (a stopped session,
    // a cancelled intent, or an in-progress stop). Used to decide the outcome
    // floor and whether a failed project-discovery phase is worth surfacing.
    const lexicalFoundSomething = outcome !== 'none';
    // Run the async project-key phase when either:
    //  - no session was stopped lexically (a project preview may still run under
    //    the discovered project key), or
    //  - a session WAS stopped but an intent is still pending — the stopped
    //    session's key can differ from the current project key (project markers
    //    changed since it started), so the project key needs its stop epoch
    //    recorded to abandon a sibling intent that predates this Stop.
    const needProjectPhase =
        !stoppedSession || (deps.runtime.hasPendingStarts?.() ?? false);
    if (needProjectPhase) {
        try {
            const quartoContext = await resolveContextForSource(uri.fsPath, deps);
            if (quartoContext.key !== lexicalKey) {
                const fallback = await deps.runtime.stopByLookup(
                    quartoContext.key,
                    uri.fsPath,
                    stopEpoch,
                );
                // Keep the strongest result of the two phases. This preserves a
                // lexical 'stopped'/'cancelled-pending'/'already-stopping' as a
                // floor so a weaker fallback cannot downgrade it — e.g. a second
                // Stop during teardown ('already-stopping') must not become a
                // misleading "no preview running" ('none') just because the
                // project-key lookup found nothing.
                outcome = strongerStopResult(outcome, fallback);
            }
        } catch (err) {
            // Project discovery can fail or hang (a wedged remote filesystem).
            // It must not discard work the lexical phase already did — a stopped
            // session, a cancelled intent, or an already-stopping session are
            // all "found something", and the project-key phase is best-effort.
            // Only surface the error when the Stop had otherwise found nothing.
            if (!lexicalFoundSomething) throw err;
        }
    }
    if (outcome === 'stopped' || outcome === 'cancelled-pending') {
        await vscode.window.showInformationMessage('Quarto preview stopped.');
    } else if (outcome === 'none') {
        await vscode.window.showInformationMessage(
            'No Quarto preview is running for this document.',
        );
    }
    // already-stopping is intentionally a silent no-op.
}

/**
 * Merge two Stop phases' results, keeping the more meaningful one so a weaker
 * later phase never masks work an earlier phase already reported. Ranked:
 * stopped a session > cancelled a pending intent > a stop already in progress
 * > nothing found.
 */
function strongerStopResult(
    a: QuartoStopResult,
    b: QuartoStopResult,
): QuartoStopResult {
    const rank: Record<QuartoStopResult, number> = {
        stopped: 3,
        'cancelled-pending': 2,
        'already-stopping': 1,
        none: 0,
    };
    return rank[a] >= rank[b] ? a : b;
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

        const context = await resolveContextForSource(uri.fsPath, deps);
        const opKey = renderGuardKey(context);
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
            completed.renderStartMs,
            deps.output,
        );
    };
}

function renderGuardKey(
    context: QuartoContext,
): string {
    const kind = context.projectRoot === null ? 'file' : 'project';
    return `${kind}:${context.key}`;
}

/**
 * Hard bound on project-context discovery. `realpath` and the ancestor
 * marker-file walk are ordinary filesystem calls that return in microseconds
 * on a local disk, but can wedge indefinitely on an unavailable network mount.
 * A hang here would otherwise stall Stop (holding deactivation up to its bound)
 * and pin a Preview intent in preflight forever; the timeout turns that
 * un-catchable hang into a rejection every caller already handles.
 */
export const QUARTO_CONTEXT_TIMEOUT_MS = 10_000;

class QuartoContextTimeoutError extends Error {
    constructor(sourceFsPath: string, timeoutMs: number) {
        super(
            `Quarto project discovery for ${sourceFsPath} timed out after ` +
            `${timeoutMs}ms (filesystem slow or unavailable).`,
        );
        this.name = 'QuartoContextTimeoutError';
    }
}

async function resolveContextForSource(
    sourceFsPath: string,
    deps: QuartoCommandDeps,
): Promise<QuartoContext> {
    const resolveContext = deps.resolveContext ?? ((candidate: string) =>
        resolveQuartoContext(candidate, {
            realpath: fs.promises.realpath,
            isProjectMarkerFile: isQuartoProjectMarkerFile,
        }));
    const timeoutMs = deps.contextTimeoutMs ?? QUARTO_CONTEXT_TIMEOUT_MS;
    const bound = cancelableDelay(timeoutMs);
    try {
        const result = await Promise.race([
            resolveContext(sourceFsPath).then((ctx) => ({ ok: true as const, ctx })),
            bound.promise.then(() => ({ ok: false as const })),
        ]);
        if (!result.ok) {
            throw new QuartoContextTimeoutError(sourceFsPath, timeoutMs);
        }
        return result.ctx;
    } finally {
        bound.cancel();
    }
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
    let renderStartMs = 0;
    const result = await vscode.window.withProgress<KnitEngineResult>(
        {
            location: vscode.ProgressLocation.Notification,
            cancellable: true,
            title: `Rendering ${path.basename(preflight.uri.fsPath)} with Quarto…`,
        },
        async (_progress, cancellation) => {
            renderStartMs = (deps.now ?? Date.now)();
            return deps.runRender({
                quartoPath,
                sourceFsPath: preflight.uri.fsPath,
                cwd: preflight.context.cwd,
                timeoutMs,
                output: deps.output,
                cancellation,
            });
        },
    );
    return { kind: 'completed', result, timeoutMs, renderStartMs, preflight };
}

async function renderOutcome(
    result: KnitEngineResult,
    preflight: QuartoPreflight,
    timeoutMs: number,
    renderStartMs: number,
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
        renderStartMs,
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
