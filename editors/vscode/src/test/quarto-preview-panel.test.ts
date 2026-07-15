import * as assert from 'assert';
import * as vscode from 'vscode';
import { activate } from './helper';
import {
    QuartoPreviewPanel,
    type QuartoPreviewPanelDeps,
} from '../quarto/quarto-preview-panel';

class Deferred {
    readonly promise: Promise<void>;
    resolve!: () => void;

    constructor() {
        this.promise = new Promise<void>((resolve) => {
            this.resolve = resolve;
        });
    }
}

suite('QuartoPreviewPanel integration', () => {
    let output: vscode.OutputChannel;
    let stops: Array<{ key: string; generation: number }>;
    let deps: QuartoPreviewPanelDeps;
    let posted: unknown[];
    let stored: Map<string, unknown>;

    suiteSetup(async () => {
        await activate();
        output = vscode.window.createOutputChannel('Quarto Panel Test');
    });

    setup(() => {
        stops = [];
        posted = [];
        stored = new Map();
        const globalState = {
            get: <T>(key: string) => stored.get(key) as T | undefined,
            update: async (key: string, value: unknown) => { stored.set(key, value); },
            keys: () => [...stored.keys()],
        } as unknown as vscode.Memento;
        deps = {
            context: {
                subscriptions: [],
                globalState,
            } as unknown as vscode.ExtensionContext,
            output,
            stopGeneration: async (key, generation) => {
                stops.push({ key, generation });
            },
            keyForSource: async (sourceFsPath) => sourceFsPath,
            postWebviewMessage: (_webview, message) => {
                posted.push(message);
                return true;
            },
        };
    });

    teardown(() => {
        QuartoPreviewPanel.disposeAllForTesting();
    });

    suiteTeardown(() => output.dispose());

    test('create and subsequent state updates reuse one panel per key', () => {
        const first = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 1,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(first);
        const second = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 1,
            sourceFsPath: '/project/a.qmd',
            rawUrl: 'http://127.0.0.1:4000/',
            state: { kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' },
        }, deps);
        assert.ok(second);
        assert.strictEqual(first, second);
        assert.strictEqual(QuartoPreviewPanel.getInstancesForTesting().size, 1);
        assert.ok(second.getPanelForTesting().webview.html.includes('raven-quarto-frame'));

        const third = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 2,
            sourceFsPath: '/project/b.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(third);
        assert.strictEqual(third, first);
        assert.ok(third.getPanelForTesting().webview.html.includes('/project/b.qmd'));
    });

    test('disposing a panel stops only its bound runtime generation', async () => {
        const stopped = new Deferred();
        deps = {
            ...deps,
            stopGeneration: async (key, generation) => {
                stops.push({ key, generation });
                stopped.resolve();
            },
        };
        const instance = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 7,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(instance);
        instance.getPanelForTesting().dispose();
        await stopped.promise;
        assert.deepStrictEqual(stops, [{ key: '/project', generation: 7 }]);
    });

    test('dispose reports a rejected generation stop at the event boundary', async () => {
        const logged = new Deferred();
        const lines: string[] = [];
        const rejectingDeps: QuartoPreviewPanelDeps = {
            ...deps,
            output: {
                appendLine: (line: string) => {
                    lines.push(line);
                    logged.resolve();
                },
            } as vscode.OutputChannel,
            stopGeneration: async () => {
                throw new Error('injected stop rejection');
            },
        };
        const instance = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 9,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'starting' },
        }, rejectingDeps);
        assert.ok(instance);

        instance.getPanelForTesting().dispose();
        await logged.promise;

        assert.deepStrictEqual(lines, [
            '[panel] stopGeneration failed: Error: injected stop rejection',
        ]);
    });

    test('rekeys and adopts the same-source panel when project identity changes', async () => {
        const stoppedTwice = new Deferred();
        deps = {
            ...deps,
            stopGeneration: async (key, generation) => {
                stops.push({ key, generation });
                if (stops.length === 2) stoppedTwice.resolve();
            },
        };
        const standaloneA = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project/a.qmd',
            generation: 4,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(standaloneA);
        QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 1,
            sourceFsPath: '/project/b.qmd',
            state: { kind: 'starting' },
        }, deps);

        const adopted = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 2,
            sourceFsPath: '/project/sub/../a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(adopted);

        assert.strictEqual(adopted, standaloneA);
        assert.strictEqual(QuartoPreviewPanel.getInstancesForTesting().size, 1);
        assert.strictEqual(
            QuartoPreviewPanel.getInstancesForTesting().get('/project'),
            standaloneA,
        );
        assert.strictEqual(adopted.getPanelForTesting().title, 'Quarto Preview: a.qmd');

        adopted.getPanelForTesting().dispose();
        await stoppedTwice.promise;
        assert.deepStrictEqual(stops, [
            { key: '/project', generation: 1 },
            { key: '/project', generation: 2 },
        ]);
    });

    test('deactivation disposes every panel and clears the static registry', async () => {
        const stoppedTwice = new Deferred();
        deps = {
            ...deps,
            stopGeneration: async (key, generation) => {
                stops.push({ key, generation });
                if (stops.length === 2) stoppedTwice.resolve();
            },
        };
        const first = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project-a',
            generation: 3,
            sourceFsPath: '/project-a/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(first);
        const second = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project-b',
            generation: 8,
            sourceFsPath: '/project-b/b.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(second);
        let disposals = 0;
        first.getPanelForTesting().onDidDispose(() => { disposals++; });
        second.getPanelForTesting().onDidDispose(() => { disposals++; });

        QuartoPreviewPanel.disposeAllForDeactivation();
        await stoppedTwice.promise;

        assert.strictEqual(QuartoPreviewPanel.getInstancesForTesting().size, 0);
        assert.strictEqual(disposals, 2);
        assert.deepStrictEqual(stops, [
            { key: '/project-a', generation: 3 },
            { key: '/project-b', generation: 8 },
        ]);
    });

    test('terminal updates without an existing panel do not create one', () => {
        const stopped = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 1,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'stopped' },
        }, deps);
        const failed = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 1,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'failed', detailText: 'failed' },
        }, deps);
        const exited = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 1,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'exited-unexpectedly', code: 1 },
        }, deps);
        const restored = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 0,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'restore-placeholder' },
        }, deps);

        assert.strictEqual(stopped, null);
        assert.strictEqual(failed, null);
        assert.strictEqual(exited, null);
        assert.strictEqual(restored, null);
        assert.strictEqual(QuartoPreviewPanel.getInstancesForTesting().size, 0);
    });

    test('stopped updates still render in an already-open panel', () => {
        const instance = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 3,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(instance);
        const serving = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 3,
            sourceFsPath: '/project/a.qmd',
            rawUrl: 'http://127.0.0.1:4000/',
            state: { kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' },
        }, deps);
        assert.strictEqual(serving, instance);
        const stopped = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 3,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'stopped' },
        }, deps);

        assert.strictEqual(stopped, instance);
        assert.strictEqual(QuartoPreviewPanel.getInstancesForTesting().size, 1);
        assert.ok(instance.getPanelForTesting().webview.html.includes(
            'Quarto preview stopped.',
        ));
        assert.ok(!instance.getPanelForTesting().webview.html.includes(
            '<iframe id="raven-quarto-frame"',
        ));
    });

    test('Open in Browser false result warns and logs the copyable URL', async () => {
        const lines: string[] = [];
        const warnings: string[] = [];
        const openedUrls: string[] = [];
        const originalOpenExternal = vscode.env.openExternal;
        const originalShowWarning = vscode.window.showWarningMessage;
        (vscode.env as { openExternal: unknown }).openExternal = async (
            uri: vscode.Uri,
        ) => {
            openedUrls.push(uri.toString());
            return false;
        };
        (vscode.window as { showWarningMessage: unknown }).showWarningMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            warnings.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const browserDeps: QuartoPreviewPanelDeps = {
                ...deps,
                output: {
                    appendLine: (line: string) => { lines.push(line); },
                } as unknown as vscode.OutputChannel,
            };
            const rawUrl = 'http://127.0.0.1:4555/proxy/chapter/';
            const browserUrl = 'http://127.0.0.1:4444/chapter/';
            const instance = QuartoPreviewPanel.applyRuntimeUpdate({
                key: '/project',
                generation: 1,
                sourceFsPath: '/project/a.qmd',
                state: { kind: 'starting' },
            }, browserDeps);
            assert.ok(instance);
            QuartoPreviewPanel.applyRuntimeUpdate({
                key: '/project',
                generation: 1,
                sourceFsPath: '/project/a.qmd',
                rawUrl,
                browserUrl,
                state: { kind: 'serving', externalUrl: rawUrl },
            }, browserDeps);

            await instance.handleMessageForTesting({ type: 'open-in-browser' });

            assert.ok(
                lines.includes(`[panel] Open in Browser failed: ${browserUrl}`),
                'browser failure remains copyable alongside theme diagnostics',
            );
            assert.deepStrictEqual(openedUrls, [browserUrl]);
            assert.deepStrictEqual(warnings, [
                'VS Code could not open the Quarto preview in a browser. ' +
                'The URL was written to Raven: Quarto output.',
            ]);
        } finally {
            (vscode.env as { openExternal: unknown }).openExternal = originalOpenExternal;
            (vscode.window as { showWarningMessage: unknown }).showWarningMessage = (
                originalShowWarning
            );
        }
    });

    test('serializer restore reapplies options, shows placeholder, and never starts', async () => {
        const panel = vscode.window.createWebviewPanel(
            'raven.quartoPreview.test.restore',
            'Restore',
            vscode.ViewColumn.Beside,
            {},
        );
        panel.webview.options = {};
        const restored = await QuartoPreviewPanel.restore(
            panel,
            { sourceFsPath: '/project/restored.qmd' },
            deps,
        );
        assert.ok(restored);
        assert.strictEqual(panel.webview.options.enableScripts, true);
        assert.deepStrictEqual(panel.webview.options.localResourceRoots, []);
        assert.ok(panel.webview.html.includes('This restored preview is not running'));
        assert.strictEqual(stops.length, 0, 'restore must not spawn or stop a runtime session');
    });

    test('serializer disposes malformed persisted state after reapplying options', async () => {
        const panel = vscode.window.createWebviewPanel(
            'raven.quartoPreview.test.malformed',
            'Malformed',
            vscode.ViewColumn.Beside,
            {},
        );
        const disposed = new Deferred();
        panel.onDidDispose(() => { disposed.resolve(); });
        const restored = await QuartoPreviewPanel.restore(
            panel,
            { sourceFsPath: 42, extra: true },
            deps,
        );
        await disposed.promise;
        assert.strictEqual(restored, null);
        assert.strictEqual(stops.length, 0);
    });

    test('serving posts a theme update and installs handshake/load handling', async () => {
        const instance = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/theme-project',
            generation: 1,
            sourceFsPath: '/theme-project/a.qmd',
            state: { kind: 'serving', externalUrl: 'http://127.0.0.1:4555/' },
        }, deps);
        assert.ok(instance);
        await waitUntil(() => posted.some(isThemeUpdate));
        const html = instance.getPanelForTesting().webview.html;
        assert.ok(html.includes('event.origin === frameOrigin && isThemeReady(message)'));
        assert.ok(html.includes("frame.addEventListener('load', function ()"));
        assert.ok(html.includes('postThemeToFrame();'));
        const readyStart = html.indexOf(
            'if (event.origin === frameOrigin && isThemeReady(message))',
        );
        const readyEnd = html.indexOf('\n                    return;', readyStart);
        assert.ok(!html.slice(readyStart, readyEnd).includes('postThemeToFrame();'));
    });

    test('toggle persists and broadcasts authoritative theme updates to two panels', async () => {
        const first = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/theme-a',
            generation: 1,
            sourceFsPath: '/theme-a/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        const second = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/theme-b',
            generation: 1,
            sourceFsPath: '/theme-b/b.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(first);
        assert.ok(second);

        await first.handleMessageForTesting({ type: 'theme-changed', applied: true });

        assert.strictEqual(stored.get('raven.quarto.applyVSCodeTheme'), true);
        const enabledUpdates = posted.filter((message): message is {
            type: 'theme-update';
            payload: { enabled: boolean };
        } => isThemeUpdate(message) && message.payload.enabled === true);
        assert.ok(enabledUpdates.length >= 2, 'both panels receive enabled=true');
    });

    test('visible resend and same-kind theme refresh re-request context and theme', async () => {
        const instance = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/theme-visible',
            generation: 1,
            sourceFsPath: '/theme-visible/a.qmd',
            state: { kind: 'serving', externalUrl: 'http://127.0.0.1:4556/' },
        }, deps);
        assert.ok(instance);
        await waitUntil(() => posted.some(isThemeUpdate));
        const beforeTheme = posted.filter(isThemeUpdate).length;

        instance.handleActiveThemeChangeForTesting();
        await waitUntil(() => posted.some(isThemeContextRequest));
        await waitUntil(() => posted.filter(isThemeUpdate).length > beforeTheme);

        const cover = vscode.window.createWebviewPanel(
            'raven.quartoPreview.test.cover',
            'Cover',
            instance.getPanelForTesting().viewColumn ?? vscode.ViewColumn.One,
            {},
        );
        try {
            const beforeVisible = posted.filter(isThemeUpdate).length;
            const beforeVisibleContext = posted.filter(isThemeContextRequest).length;
            instance.getPanelForTesting().reveal(
                instance.getPanelForTesting().viewColumn ?? vscode.ViewColumn.One,
                false,
            );
            await waitUntil(() => posted.filter(isThemeUpdate).length > beforeVisible);
            await waitUntil(() =>
                posted.filter(isThemeContextRequest).length > beforeVisibleContext
            );
        } finally {
            cover.dispose();
        }
    });
});

function isThemeUpdate(message: unknown): message is {
    type: 'theme-update';
    payload: { enabled: boolean };
} {
    return message !== null
        && typeof message === 'object'
        && (message as { type?: unknown }).type === 'theme-update';
}

function isThemeContextRequest(message: unknown): boolean {
    return message !== null
        && typeof message === 'object'
        && (message as { type?: unknown }).type === 'theme-context-request';
}

async function waitUntil(predicate: () => boolean, timeoutMs = 5000): Promise<void> {
    const started = Date.now();
    while (!predicate()) {
        if (Date.now() - started > timeoutMs) {
            throw new Error('timed out waiting for Quarto panel theme event');
        }
        await new Promise((resolve) => setTimeout(resolve, 20));
    }
}
