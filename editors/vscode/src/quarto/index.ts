/**
 * Activation wiring for Raven's standalone Quarto workflow.
 *
 * One activation lifecycle owns the shared output channel, preview runtime,
 * one-shot render engine, and preview panels in this extension-host window.
 * Its single awaited shutdown rejects new command continuations and process
 * starts, claims child teardown before disposing panels that retain runtime
 * callbacks, then disposes output only after the bounded command + engine
 * aggregates settle. The output facade makes any continuation abandoned by a
 * bound harmless after disposal. The webview serializer is registered at most
 * once, and its guard is reset when the registration is disposed so a disable
 * / enable cycle in the same JS process can register a fresh serializer.
 */

import * as fs from 'fs';
import * as vscode from 'vscode';
import { registerQuartoCommands } from './quarto-commands';
import { loadQuartoPreviewBridgeAssets } from './quarto-bridge-assets';
import type { QuartoCommandLifecycle } from './quarto-command-lifecycle';
import { probeQuartoBinary, QuartoResolver } from './quarto-detect';
import { QuartoPreviewProcess } from './quarto-preview-engine';
import { QuartoPreviewPanel, type QuartoPreviewPanelDeps } from './quarto-preview-panel';
import { QuartoRuntime } from './quarto-preview-runtime';
import { QuartoPreviewWithProxyProcess } from './quarto-preview-with-proxy';
import { resolveQuartoContext } from './quarto-project';
import { isQuartoProjectMarkerFile } from './quarto-project-fs';
import { QuartoRenderEngine } from './quarto-render-engine';
import { createSafeQuartoOutputChannel } from './quarto-output';

interface QuartoLifecycle {
    runtime: QuartoRuntime;
    renderEngine: QuartoRenderEngine;
    commands: QuartoCommandLifecycle;
    output: vscode.OutputChannel;
    shutdownPromise: Promise<void> | null;
}

let activeLifecycle: QuartoLifecycle | null = null;
let serializerRegistration: vscode.Disposable | null = null;

export function registerQuarto(context: vscode.ExtensionContext): void {
    const output = createSafeQuartoOutputChannel(
        vscode.window.createOutputChannel('Raven: Quarto'),
    );
    const resolver = new QuartoResolver<vscode.Uri>({
        getConfigured: (resource) => vscode.workspace
            .getConfiguration('raven.quarto', resource)
            .get<string>('path', ''),
        access: (candidate) => fs.promises.access(candidate, fs.constants.X_OK),
        probe: probeQuartoBinary,
    });
    const renderEngine = new QuartoRenderEngine();
    const bridgeAssets = loadQuartoPreviewBridgeAssets(
        context.extensionUri.fsPath,
        output,
    );

    let runtime!: QuartoRuntime;
    const panelDeps: QuartoPreviewPanelDeps = {
        context,
        output,
        stopGeneration: (key, generation) => runtime.stopGeneration(key, generation),
        keyForSource: async (sourceFsPath) => (
            await resolveQuartoContext(sourceFsPath, {
                realpath: fs.promises.realpath,
                isProjectMarkerFile: isQuartoProjectMarkerFile,
            })
        ).key,
    };
    runtime = new QuartoRuntime({
        processFactory: (args) => new QuartoPreviewWithProxyProcess({
            output,
            bridgeAssets,
            onUnexpectedExit: args.onUnexpectedExit,
            createInner: (onUnexpectedExit) => new QuartoPreviewProcess({
                quartoPath: args.quartoPath,
                sourceFsPath: args.sourceFsPath,
                cwd: args.cwd,
                output,
                onUnexpectedExit,
            }),
        }),
        asExternalUri: async (rawUrl) => (
            await vscode.env.asExternalUri(vscode.Uri.parse(rawUrl))
        ).toString(),
        onViewUpdate: (update) => {
            QuartoPreviewPanel.applyRuntimeUpdate(update, panelDeps);
        },
        onLifecycleError: (message) => {
            try { output.appendLine(message); } catch { /* disposing */ }
        },
    });
    const commands = registerQuartoCommands(context, {
        resolver,
        runtime,
        output,
        runRender: (opts) => renderEngine.run(opts),
    });
    const lifecycle: QuartoLifecycle = {
        runtime,
        renderEngine,
        commands,
        output,
        shutdownPromise: null,
    };
    activeLifecycle = lifecycle;
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((event) => {
            if (event.affectsConfiguration('raven.quarto.path')) resolver.invalidate();
        }),
        {
            dispose: () => { void shutdownQuartoLifecycle(lifecycle); },
        },
    );

    if (serializerRegistration === null) {
        const registration = vscode.window.registerWebviewPanelSerializer(
            'raven.quartoPreview',
            {
                deserializeWebviewPanel: async (panel, state) => {
                    await QuartoPreviewPanel.restore(panel, state, panelDeps);
                },
            },
        );
        let disposed = false;
        const guarded: vscode.Disposable = {
            dispose: () => {
                if (disposed) return;
                disposed = true;
                registration.dispose();
                if (serializerRegistration === guarded) serializerRegistration = null;
            },
        };
        serializerRegistration = guarded;
        context.subscriptions.push(guarded);
    }
}

/**
 * Await bounded command + process shutdown and clear module-persistent panels,
 * serializer state, output, and lifecycle references.
 */
export async function stopAllQuartoForDeactivation(): Promise<void> {
    const lifecycle = activeLifecycle;
    if (lifecycle) await shutdownQuartoLifecycle(lifecycle);
}

function shutdownQuartoLifecycle(lifecycle: QuartoLifecycle): Promise<void> {
    if (lifecycle.shutdownPromise) return lifecycle.shutdownPromise;

    // Reject new commands first, then set both engines deactivating and claim
    // their shared per-child teardown promises before panel disposal can
    // request a generation stop.
    const commands = lifecycle.commands.shutdown();
    const previews = lifecycle.runtime.shutdown();
    const renders = lifecycle.renderEngine.shutdown();

    if (activeLifecycle === lifecycle) {
        serializerRegistration?.dispose();
        serializerRegistration = null;
        QuartoPreviewPanel.disposeAllForDeactivation();
    }

    lifecycle.shutdownPromise = Promise.allSettled([
        commands,
        previews,
        renders,
    ]).then(() => {
        lifecycle.output.dispose();
        if (activeLifecycle === lifecycle) activeLifecycle = null;
    });
    return lifecycle.shutdownPromise;
}

export {
    QuartoNotFoundError,
    QuartoResolver,
} from './quarto-detect';
