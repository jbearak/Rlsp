/**
 * Per-key Quarto preview panels and serializer restoration.
 *
 * Host state is authoritative. Hidden webviews may drop `postMessage`, so the
 * current state is re-posted on `webview-ready` and whenever a panel becomes
 * visible. Transitions that install/remove an iframe rebuild the shell; every
 * serving installation receives the fresh `asExternalUri` mapping produced by
 * the matching runtime generation, and CSP + iframe src are derived together.
 * An unexpected exit is the sole serving-to-non-serving transition that keeps
 * the existing iframe for inspection.
 *
 * The registry is keyed by the current project-or-file key, but source paths
 * are a second identity axis: when `_quarto.yml` appears or disappears, an
 * existing panel for the same canonical source is rekeyed and adopted. This
 * takes precedence over a different-source panel currently occupying the new
 * key, which is disposed as a stale generation. The adopted panel's mutable
 * key means its eventual disposal stops the newly-bound generation.
 * A terminal update can update or rekey an existing panel, but never creates
 * one after disposal; only `starting` and defensive `serving` updates may open
 * a new panel. This prevents a dispose-triggered `stopped` emit from reopening
 * the tab it just closed.
 *
 * Restored panels reapply `webview.options` before any other work because VS
 * Code does not persist them. Restoration validates `{ sourceFsPath }`, awaits
 * async physical project-key discovery, wires the inert panel, and renders a
 * placeholder; it never starts Quarto. Panel
 * disposal stops only the generation that panel currently represents.
 * Async message and dispose-stop handling are caught at their event boundaries
 * so rejected VS Code commands, URI opens, or runtime stops are reported
 * without becoming unhandled host rejections. A false Open-in-Browser result
 * warns the user and records the raw validated URL for manual copying.
 * Deactivation disposes every panel and clears the static registry because VS
 * Code keeps JS modules alive across disable/enable cycles; retaining a panel
 * would retain the old runtime dependencies and generation counters too.
 */

import * as crypto from 'crypto';
import * as path from 'path';
import * as vscode from 'vscode';
import { canonicalOpKey } from '../knit/raven-knit-paths';
import { applyViewerTabIcon } from '../viewer-tab-icon';
import type { QuartoPreviewViewState } from './quarto-messages';
import { isPreviewToExtensionMessage } from './quarto-messages';
import { buildQuartoPreviewShellHtml } from './quarto-preview-html';
import type { QuartoRuntimeViewUpdate } from './quarto-preview-runtime';

export interface QuartoPreviewPanelDeps {
    output: vscode.OutputChannel;
    stopGeneration(key: string, generation: number): Promise<unknown>;
    keyForSource(sourceFsPath: string): Promise<string>;
}

type ViewerColumnSetting = 'active' | 'beside';

export class QuartoPreviewPanel {
    private static readonly instances = new Map<string, QuartoPreviewPanel>();

    private generation: number;
    private sourceFsPath: string;
    private state: QuartoPreviewViewState;
    private rawUrl: string | undefined;
    private frameInstalled = false;
    private disposed = false;

    private constructor(
        private readonly panel: vscode.WebviewPanel,
        private key: string,
        generation: number,
        sourceFsPath: string,
        state: QuartoPreviewViewState,
        private readonly deps: QuartoPreviewPanelDeps,
    ) {
        this.generation = generation;
        this.sourceFsPath = sourceFsPath;
        this.state = state;
        this.wire();
        this.rebuildHtml();
    }

    static applyRuntimeUpdate(
        update: QuartoRuntimeViewUpdate,
        deps: QuartoPreviewPanelDeps,
    ): QuartoPreviewPanel | null {
        const existing = this.instances.get(update.key);
        const sourceKey = canonicalOpKey({ fsPath: update.sourceFsPath });
        const bySource = [...this.instances.values()].find((candidate) =>
            !candidate.disposed
            && candidate.key !== update.key
            && canonicalOpKey({ fsPath: candidate.sourceFsPath }) === sourceKey,
        );
        if (bySource) {
            // The new key may currently belong to a different source in the
            // same project. Its generation is stale relative to this update;
            // disposal preserves the normal generation-checked stop contract.
            if (existing && existing !== bySource) existing.panel.dispose();
            if (this.instances.get(bySource.key) === bySource) {
                this.instances.delete(bySource.key);
            }
            bySource.adoptRekeyedUpdate(update);
            this.instances.set(update.key, bySource);
            bySource.panel.reveal(bySource.panel.viewColumn, true);
            return bySource;
        }
        if (existing) {
            if (existing.applyUpdate(update)) {
                existing.panel.reveal(existing.panel.viewColumn, true);
            }
            return existing;
        }
        if (update.state.kind !== 'starting' && update.state.kind !== 'serving') {
            return null;
        }

        const sourceUri = vscode.Uri.file(update.sourceFsPath);
        const configured = vscode.workspace
            .getConfiguration('raven.quarto', sourceUri)
            .get<ViewerColumnSetting>('viewerColumn', 'beside');
        const column = configured === 'active'
            ? vscode.ViewColumn.Active
            : vscode.ViewColumn.Beside;
        const panel = vscode.window.createWebviewPanel(
            'raven.quartoPreview',
            `Quarto Preview: ${path.basename(update.sourceFsPath)}`,
            { viewColumn: column, preserveFocus: true },
            {
                enableScripts: true,
                localResourceRoots: [],
                retainContextWhenHidden: true,
            },
        );
        applyViewerTabIcon(panel, 'preview');
        const instance = new QuartoPreviewPanel(
            panel,
            update.key,
            update.generation,
            update.sourceFsPath,
            update.state,
            deps,
        );
        instance.rawUrl = update.rawUrl;
        this.instances.set(update.key, instance);
        // The constructor rendered before rawUrl assignment, but rawUrl does
        // not enter HTML; it is retained only for Open in Browser.
        return instance;
    }

    /**
     * Adopt a serialized panel without starting a preview process.
     * `webview.options` is intentionally the first mutation.
     */
    static async restore(
        panel: vscode.WebviewPanel,
        persistedState: unknown,
        deps: QuartoPreviewPanelDeps,
    ): Promise<QuartoPreviewPanel | null> {
        panel.webview.options = {
            enableScripts: true,
            localResourceRoots: [],
        };

        if (!isPersistedState(persistedState)) {
            panel.dispose();
            return null;
        }
        const key = await deps.keyForSource(persistedState.sourceFsPath);
        const existing = this.instances.get(key);
        if (existing && !existing.disposed) {
            existing.panel.reveal(existing.panel.viewColumn, true);
            panel.dispose();
            return existing;
        }

        applyViewerTabIcon(panel, 'preview');
        const instance = new QuartoPreviewPanel(
            panel,
            key,
            0,
            persistedState.sourceFsPath,
            { kind: 'restore-placeholder' },
            deps,
        );
        this.instances.set(key, instance);
        return instance;
    }

    /** Test-only registry view. */
    static getInstancesForTesting(): ReadonlyMap<string, QuartoPreviewPanel> {
        return this.instances;
    }

    /** Dispose panels and module-persistent registry state on deactivation. */
    static disposeAllForDeactivation(): void {
        for (const instance of [...this.instances.values()]) {
            try { instance.panel.dispose(); } catch { /* already disposed */ }
        }
        this.instances.clear();
    }

    /** Test-only cleanup of real panels and registry state. */
    static disposeAllForTesting(): void {
        this.disposeAllForDeactivation();
    }

    /** Test-only access to the underlying VS Code panel. */
    getPanelForTesting(): vscode.WebviewPanel {
        return this.panel;
    }

    /** Test-only message entry point for browser-open outcome coverage. */
    async handleMessageForTesting(message: unknown): Promise<void> {
        await this.handleMessage(message);
    }

    private applyUpdate(update: QuartoRuntimeViewUpdate): boolean {
        if (update.generation < this.generation || this.disposed) return false;
        let sourceChanged = false;
        if (update.generation > this.generation) {
            this.generation = update.generation;
            sourceChanged = this.sourceFsPath !== update.sourceFsPath;
            this.sourceFsPath = update.sourceFsPath;
            this.rawUrl = undefined;
            this.panel.title = `Quarto Preview: ${path.basename(update.sourceFsPath)}`;
        }
        if (update.rawUrl !== undefined) this.rawUrl = update.rawUrl;

        const next = update.state;
        const rebuild = sourceChanged
            || next.kind === 'serving'
            || (this.frameInstalled && next.kind !== 'exited-unexpectedly');
        this.state = next;
        if (rebuild) this.rebuildHtml();
        else this.postState();
        return true;
    }

    /** Adopt an update whose generation belongs to a different key domain. */
    private adoptRekeyedUpdate(update: QuartoRuntimeViewUpdate): void {
        this.key = update.key;
        this.generation = update.generation;
        this.sourceFsPath = update.sourceFsPath;
        this.rawUrl = update.rawUrl;
        this.state = update.state;
        this.panel.title = `Quarto Preview: ${path.basename(update.sourceFsPath)}`;
        this.rebuildHtml();
    }

    private wire(): void {
        this.panel.webview.onDidReceiveMessage((message: unknown) => {
            void this.handleMessage(message).catch((err) => {
                this.deps.output.appendLine(
                    `[panel] message handler failed: ${errorMessage(err)}`,
                );
            });
        });
        this.panel.onDidChangeViewState((event) => {
            if (event.webviewPanel.visible) this.postState();
        });
        this.panel.onDidDispose(() => {
            if (this.disposed) return;
            this.disposed = true;
            if (QuartoPreviewPanel.instances.get(this.key) === this) {
                QuartoPreviewPanel.instances.delete(this.key);
            }
            void this.deps.stopGeneration(this.key, this.generation).catch((err) => {
                this.deps.output.appendLine(
                    '[panel] stopGeneration failed: ' + String(err),
                );
            });
        });
    }

    private async handleMessage(message: unknown): Promise<void> {
        if (!isPreviewToExtensionMessage(message)) {
            this.deps.output.appendLine('[panel] ignored malformed Quarto preview message');
            return;
        }
        switch (message.type) {
            case 'webview-ready':
                this.postState();
                return;
            case 'open-in-browser':
                if (this.rawUrl) {
                    const opened = await vscode.env.openExternal(
                        vscode.Uri.parse(this.rawUrl),
                    );
                    if (!opened) {
                        this.deps.output.appendLine(
                            `[panel] Open in Browser failed: ${this.rawUrl}`,
                        );
                        await vscode.window.showWarningMessage(
                            'VS Code could not open the Quarto preview in a browser. ' +
                            'The URL was written to Raven: Quarto output.',
                        );
                    }
                }
                return;
            case 'stop-preview':
                await vscode.commands.executeCommand(
                    'raven.quarto.stopPreview',
                    vscode.Uri.file(this.sourceFsPath),
                );
                return;
            case 'request-restart':
                await vscode.commands.executeCommand(
                    'raven.quarto.preview',
                    vscode.Uri.file(this.sourceFsPath),
                );
                return;
            case 'load-timeout':
                this.deps.output.appendLine(
                    `[panel] iframe load-timeout advisory for ${this.sourceFsPath}`,
                );
                return;
            case 'report-error':
                this.deps.output.appendLine(`[panel] webview error: ${message.message}`);
                return;
        }
    }

    private rebuildHtml(): void {
        if (this.disposed) return;
        this.frameInstalled = this.state.kind === 'serving';
        this.panel.webview.html = buildQuartoPreviewShellHtml({
            nonce: crypto.randomBytes(16).toString('base64'),
            sourceFsPath: this.sourceFsPath,
            state: this.state,
        });
    }

    private postState(): void {
        if (this.disposed) return;
        void Promise.resolve(this.panel.webview.postMessage({
            type: 'state-update',
            payload: this.state,
        })).catch(() => {
            // Hidden/disposed webviews can drop or reject delivery. Host state
            // remains authoritative and will be sent again when visible/ready.
        });
    }
}

function isPersistedState(value: unknown): value is { sourceFsPath: string } {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
    const record = value as Record<string, unknown>;
    return Object.keys(record).length === 1
        && typeof record.sourceFsPath === 'string'
        && record.sourceFsPath.length > 0;
}

function errorMessage(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
}
