import * as assert from 'assert';
import * as vscode from 'vscode';
import { activate, sleep } from './helper';
import {
    QuartoPreviewPanel,
    type QuartoPreviewPanelDeps,
} from '../quarto/quarto-preview-panel';

suite('QuartoPreviewPanel integration', () => {
    let output: vscode.OutputChannel;
    let stops: Array<{ key: string; generation: number }>;
    let deps: QuartoPreviewPanelDeps;

    suiteSetup(async () => {
        await activate();
        output = vscode.window.createOutputChannel('Quarto Panel Test');
    });

    setup(() => {
        stops = [];
        deps = {
            output,
            stopGeneration: async (key, generation) => {
                stops.push({ key, generation });
            },
            keyForSource: (sourceFsPath) => sourceFsPath,
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
        const instance = QuartoPreviewPanel.applyRuntimeUpdate({
            key: '/project',
            generation: 7,
            sourceFsPath: '/project/a.qmd',
            state: { kind: 'starting' },
        }, deps);
        assert.ok(instance);
        instance.getPanelForTesting().dispose();
        await sleep(10);
        assert.deepStrictEqual(stops, [{ key: '/project', generation: 7 }]);
    });

    test('rekeys and adopts the same-source panel when project identity changes', async () => {
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
        await sleep(10);
        assert.deepStrictEqual(stops, [
            { key: '/project', generation: 1 },
            { key: '/project', generation: 2 },
        ]);
    });

    test('deactivation disposes every panel and clears the static registry', async () => {
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
        await sleep(10);

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

    test('serializer restore reapplies options, shows placeholder, and never starts', () => {
        const panel = vscode.window.createWebviewPanel(
            'raven.quartoPreview.test.restore',
            'Restore',
            vscode.ViewColumn.Beside,
            {},
        );
        panel.webview.options = {};
        const restored = QuartoPreviewPanel.restore(
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
        let disposed = false;
        panel.onDidDispose(() => { disposed = true; });
        const restored = QuartoPreviewPanel.restore(
            panel,
            { sourceFsPath: 42, extra: true },
            deps,
        );
        await sleep(10);
        assert.strictEqual(restored, null);
        assert.strictEqual(disposed, true);
        assert.strictEqual(stops.length, 0);
    });
});
