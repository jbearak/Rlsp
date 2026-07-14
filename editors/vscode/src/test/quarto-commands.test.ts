import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';
import type { KnitEngineResult } from '../knit/knit-engine';
import { QuartoNotFoundError } from '../quarto/quarto-detect';
import { activate, awaitActive } from './helper';
import {
    createQuartoPreviewRunnerForTesting,
    createQuartoRenderRunnerForTesting,
    runQuartoPreflightForTesting,
    runQuartoStopForTesting,
    type QuartoCommandDeps,
} from '../quarto/quarto-commands';

class Deferred<T> {
    readonly promise: Promise<T>;
    resolve!: (value: T) => void;

    constructor() {
        this.promise = new Promise<T>((resolve) => {
            this.resolve = resolve;
        });
    }
}

suite('Quarto command preflight', () => {
    let output: vscode.OutputChannel;

    suiteSetup(async () => {
        await activate();
        output = vscode.window.createOutputChannel('Quarto Command Test');
    });

    suiteTeardown(() => output.dispose());

    test('workspace trust gate offers Manage Workspace Trust', async () => {
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const result = await runQuartoPreflightForTesting(
                vscode.Uri.file('/tmp/trust.qmd'),
                'Preview',
                fakeDeps({ isWorkspaceTrusted: () => false }),
            );
            assert.strictEqual(result, null);
            assert.ok(messages.some((message) => message.includes('untrusted workspaces')));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('non-file qmd URI is rejected before any document code can run', async () => {
        const messages: string[] = [];
        let opened = false;
        let resolved = false;
        let rendered = false;
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.parse('git:/repo/doc.qmd?ref');
            const result = await runQuartoPreflightForTesting(uri, 'Render', fakeDeps({
                resolver: {
                    resolve: async () => {
                        resolved = true;
                        return 'quarto';
                    },
                },
                openTextDocument: async () => {
                    opened = true;
                    throw new Error('virtual document must not open');
                },
                runRender: async () => {
                    rendered = true;
                    throw new Error('virtual document must not render');
                },
            }));

            assert.strictEqual(result, null);
            assert.deepStrictEqual(messages, [
                "Raven: Quarto Render needs a saved .qmd file on disk; " +
                "this editor (git) isn't a file.",
            ]);
            assert.strictEqual(opened, false);
            assert.strictEqual(resolved, false);
            assert.strictEqual(rendered, false);
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('dirty document is saved before context resolution', async () => {
        const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-quarto-save-'));
        const sourcePath = path.join(tmp, 'save-before-run.qMd');
        fs.writeFileSync(sourcePath, '---\ntitle: Save\n---\n\nBefore\n', 'utf8');
        const uri = vscode.Uri.file(sourcePath);
        const doc = await vscode.workspace.openTextDocument(uri);
        const editor = await vscode.window.showTextDocument(doc);
        await awaitActive(editor);
        await editor.edit((edit) => edit.insert(
            doc.lineAt(doc.lineCount - 1).range.end,
            '\nSaved marker\n',
        ));
        assert.strictEqual(doc.isDirty, true);

        let resolvedAfterSave = false;
        try {
            const result = await runQuartoPreflightForTesting(uri, 'Render', fakeDeps({
                resolveContext: () => {
                    resolvedAfterSave = fs.readFileSync(sourcePath, 'utf8').includes('Saved marker');
                    return { key: sourcePath, cwd: tmp, projectRoot: null };
                },
            }));
            assert.ok(result);
            assert.strictEqual(resolvedAfterSave, true);
            assert.strictEqual(doc.isDirty, false);
        } finally {
            await awaitActive(editor);
            await vscode.commands.executeCommand('workbench.action.closeActiveEditor');
            try { fs.rmSync(tmp, { recursive: true, force: true }); } catch { /* cleanup */ }
        }
    });

    test('server shiny frontmatter is rejected with a clear message', async () => {
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/tmp/shiny.qmd');
            const result = await runQuartoPreflightForTesting(uri, 'Preview', fakeDeps({
                openTextDocument: async () => fakeDocument(uri, '---\nserver: shiny\n---\n'),
            }));
            assert.strictEqual(result, null);
            assert.ok(messages.some((message) =>
                message.includes('server: shiny') && message.includes('quarto serve')));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('mixed-case .qMd is accepted', async () => {
        const uri = vscode.Uri.file('/tmp/mixed.qMd');
        const result = await runQuartoPreflightForTesting(uri, 'Preview', fakeDeps({
            openTextDocument: async () => fakeDocument(uri, '# document'),
        }));
        assert.ok(result);
        assert.strictEqual(result.uri.fsPath, uri.fsPath);
    });

    test('Stop with nothing running shows info and bypasses every preflight dependency', async () => {
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            await runQuartoStopForTesting(
                vscode.Uri.file('/tmp/stopped.qmd'),
                fakeDeps({
                    isWorkspaceTrusted: () => { throw new Error('trust must not run'); },
                    openTextDocument: async () => { throw new Error('open must not run'); },
                    runtime: {
                        registerPendingStart: () => { throw new Error('register must not run'); },
                        reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                        consumePendingStart: () => { throw new Error('consume must not run'); },
                        releasePendingStart: () => { throw new Error('release must not run'); },
                        startOrRestart: async () => { throw new Error('start must not run'); },
                        stopByLookup: async () => 'none',
                    } as QuartoCommandDeps['runtime'],
                }),
            );
            assert.ok(messages.includes('No Quarto preview is running for this document.'));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('Stop on a non-file editor degrades to no matching preview', async () => {
        const messages: string[] = [];
        const lookups: Array<{ key: string; source: string }> = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            await runQuartoStopForTesting(
                vscode.Uri.parse('git:/repo/doc.qmd?ref'),
                fakeDeps({
                    resolveContext: (sourceFsPath) => ({
                        key: sourceFsPath,
                        cwd: path.dirname(sourceFsPath),
                        projectRoot: null,
                    }),
                    runtime: {
                        registerPendingStart: () => { throw new Error('register must not run'); },
                        reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                        consumePendingStart: () => { throw new Error('consume must not run'); },
                        releasePendingStart: () => { throw new Error('release must not run'); },
                        startOrRestart: async () => {
                            throw new Error('start must not run');
                        },
                        stopByLookup: async (key, source) => {
                            lookups.push({ key, source });
                            return 'none';
                        },
                    } as QuartoCommandDeps['runtime'],
                }),
            );

            assert.deepStrictEqual(lookups, [{
                key: '/repo/doc.qmd',
                source: '/repo/doc.qmd',
            }]);
            assert.deepStrictEqual(messages, [
                'No Quarto preview is running for this document.',
            ]);
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('Stop during slow Preview resolution cancels the pending launch', async () => {
        const resolveStarted = new Deferred<void>();
        const finishResolve = new Deferred<string>();
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        let pendingActive = false;
        let launches = 0;
        const runtime = {
            registerPendingStart: (sourceFsPath: string) => {
                pendingActive = true;
                return { id: 1, sourceFsPath };
            },
            reconcilePendingStart: () => pendingActive,
            consumePendingStart: () => {
                const active = pendingActive;
                pendingActive = false;
                return active;
            },
            releasePendingStart: () => { pendingActive = false; },
            startOrRestart: async () => {
                launches++;
                return { kind: 'superseded' as const, generation: 1 };
            },
            stopByLookup: async () => {
                const stopped = pendingActive;
                pendingActive = false;
                return stopped ? 'stopped' as const : 'none' as const;
            },
        };
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/pending.qmd');
            const deps = fakeDeps({
                runtime,
                resolver: {
                    resolve: async () => {
                        resolveStarted.resolve(undefined);
                        return finishResolve.promise;
                    },
                },
                resolveContext: () => ({
                    key: '/project',
                    cwd: '/project',
                    projectRoot: '/project',
                }),
            });
            const runPreview = createQuartoPreviewRunnerForTesting(deps);
            const starting = runPreview(uri);
            await resolveStarted.promise;

            await runQuartoStopForTesting(uri, deps);
            finishResolve.resolve('quarto');
            await starting;

            assert.strictEqual(launches, 0);
            assert.ok(messages.includes('Quarto preview stopped.'));
        } finally {
            finishResolve.resolve('quarto');
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('deactivation Preview rejection and disposed output stay silent', async () => {
        const original = vscode.window.showErrorMessage;
        let errors = 0;
        (vscode.window as { showErrorMessage: unknown }).showErrorMessage = (
            _message: string,
        ): Thenable<string | undefined> => {
            errors++;
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/deactivating.qmd');
            const deps = fakeDeps({
                output: {
                    appendLine: () => { throw new Error('disposed output'); },
                } as unknown as vscode.OutputChannel,
                runtime: {
                    registerPendingStart: (sourceFsPath) => ({ id: 1, sourceFsPath }),
                    reconcilePendingStart: () => true,
                    consumePendingStart: () => true,
                    releasePendingStart: () => undefined,
                    startOrRestart: async () => {
                        throw new Error(
                            'Quarto runtime is deactivating; new previews are disabled.',
                        );
                    },
                    stopByLookup: async () => 'none',
                },
            });

            await createQuartoPreviewRunnerForTesting(deps)(uri);
            assert.strictEqual(errors, 0);
        } finally {
            (vscode.window as { showErrorMessage: unknown }).showErrorMessage = original;
        }
    });

    test('render key is released before the outcome notification settles', async () => {
        const firstToastShown = new Deferred<void>();
        const dismissFirstToast = new Deferred<string | undefined>();
        const original = vscode.window.showInformationMessage;
        let toastCalls = 0;
        let renderCalls = 0;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            _message: string,
        ): Thenable<string | undefined> => {
            toastCalls++;
            if (toastCalls === 1) {
                firstToastShown.resolve(undefined);
                return dismissFirstToast.promise;
            }
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/tmp/render-lock.qmd');
            const deps = fakeDeps({
                runRender: async () => {
                    renderCalls++;
                    return {
                        exitCode: 0,
                        stdout: '',
                        stderr: '',
                        cancelled: false,
                        timedOut: false,
                        spawnError: null,
                    };
                },
            });
            const runRender = createQuartoRenderRunnerForTesting(deps);

            const first = runRender(uri);
            await firstToastShown.promise;
            assert.strictEqual(renderCalls, 1);

            await runRender(uri);
            assert.strictEqual(renderCalls, 2);

            dismissFirstToast.resolve(undefined);
            await first;
        } finally {
            dismissFirstToast.resolve(undefined);
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('busy render guard runs before document open and save', async () => {
        const renderStarted = new Deferred<void>();
        const finishRender = new Deferred<KnitEngineResult>();
        let opens = 0;
        let saves = 0;
        const uri = vscode.Uri.file('/tmp/early-render-guard.qmd');
        const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
            openTextDocument: async () => {
                opens++;
                return {
                    ...fakeDocument(uri, '# document'),
                    isDirty: true,
                    save: async () => {
                        saves++;
                        return true;
                    },
                } as vscode.TextDocument;
            },
            runRender: async () => {
                renderStarted.resolve(undefined);
                return finishRender.promise;
            },
        }));

        const first = runRender(uri);
        await renderStarted.promise;
        await runRender(uri);

        assert.strictEqual(opens, 1);
        assert.strictEqual(saves, 1);
        finishRender.resolve(successfulRenderResult());
        await first;
    });

    test('realpath aliases share one in-flight render guard', async () => {
        const renderStarted = new Deferred<void>();
        const finishRender = new Deferred<KnitEngineResult>();
        let renders = 0;
        const real = vscode.Uri.file('/project/real/doc.qmd');
        const alias = vscode.Uri.file('/project/link/doc.qmd');
        const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
            realpath: () => real.fsPath,
            runRender: async () => {
                renders++;
                renderStarted.resolve(undefined);
                return finishRender.promise;
            },
        }));

        const first = runRender(real);
        await renderStarted.promise;
        await runRender(alias);

        assert.strictEqual(renders, 1);
        finishRender.resolve(successfulRenderResult());
        await first;
    });

    test('different files in one Quarto project share the render guard', async () => {
        const renderStarted = new Deferred<void>();
        const finishRender = new Deferred<KnitEngineResult>();
        let renders = 0;
        let opens = 0;
        const firstUri = vscode.Uri.file('/project/chapters/a.qmd');
        const secondUri = vscode.Uri.file('/project/chapters/b.qmd');
        const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
            resolveContext: () => ({
                key: '/project',
                cwd: '/project',
                projectRoot: '/project',
            }),
            openTextDocument: async (uri) => {
                opens++;
                return fakeDocument(uri, '# document');
            },
            runRender: async () => {
                renders++;
                renderStarted.resolve(undefined);
                return finishRender.promise;
            },
        }));

        const first = runRender(firstUri);
        await renderStarted.promise;
        await runRender(secondUri);

        assert.strictEqual(renders, 1);
        assert.strictEqual(opens, 1);
        finishRender.resolve(successfulRenderResult());
        await first;
    });

    test('symlink aliases of one Quarto project share the render guard', async () => {
        const renderStarted = new Deferred<void>();
        const finishRender = new Deferred<KnitEngineResult>();
        let renders = 0;
        const realRoot = '/real/project';
        const firstUri = vscode.Uri.file('/aliases/one/project/a.qmd');
        const secondUri = vscode.Uri.file('/aliases/two/project/b.qmd');
        const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
            resolveContext: (sourceFsPath) => {
                const projectRoot = path.dirname(sourceFsPath);
                return { key: projectRoot, cwd: projectRoot, projectRoot };
            },
            realpath: () => realRoot,
            runRender: async () => {
                renders++;
                renderStarted.resolve(undefined);
                return finishRender.promise;
            },
        }));

        const first = runRender(firstUri);
        await renderStarted.promise;
        await runRender(secondUri);

        assert.strictEqual(renders, 1);
        finishRender.resolve(successfulRenderResult());
        await first;
    });

    test('different projects and standalone files render concurrently', async () => {
        const runPair = async (projectRoots: boolean): Promise<void> => {
            const bothStarted = new Deferred<void>();
            const finishRender = new Deferred<KnitEngineResult>();
            let renders = 0;
            const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
                resolveContext: (sourceFsPath) => {
                    const parent = path.dirname(sourceFsPath);
                    return {
                        key: projectRoots ? parent : sourceFsPath,
                        cwd: parent,
                        projectRoot: projectRoots ? parent : null,
                    };
                },
                realpath: (sourceFsPath) => sourceFsPath,
                runRender: async () => {
                    renders++;
                    if (renders === 2) bothStarted.resolve(undefined);
                    return finishRender.promise;
                },
            }));
            const first = runRender(vscode.Uri.file('/project-a/a.qmd'));
            const second = runRender(vscode.Uri.file('/project-b/b.qmd'));
            await bothStarted.promise;
            assert.strictEqual(renders, 2);
            finishRender.resolve(successfulRenderResult());
            await Promise.all([first, second]);
        };

        await runPair(true);
        await runPair(false);
    });

    test('configured-path resolver error is surfaced with install actions', async () => {
        const prompts: Array<{ message: string; actions: string[] }> = [];
        const original = vscode.window.showErrorMessage;
        (vscode.window as { showErrorMessage: unknown }).showErrorMessage = (
            message: string,
            ...actions: string[]
        ): Thenable<string | undefined> => {
            prompts.push({ message, actions });
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/tmp/configured-path.qmd');
            const configuredError = new QuartoNotFoundError(
                'Configured Quarto path is unusable or is not Quarto: /bad/quarto',
            );
            const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
                resolver: { resolve: async () => { throw configuredError; } },
            }));

            await runRender(uri);

            assert.deepStrictEqual(prompts, [{
                message: configuredError.message,
                actions: ['Install…', 'Set Path…'],
            }]);
        } finally {
            (vscode.window as { showErrorMessage: unknown }).showErrorMessage = original;
        }
    });

    test('shutdown-classified cancellation produces no failure toast', async () => {
        let errors = 0;
        const original = vscode.window.showErrorMessage;
        (vscode.window as { showErrorMessage: unknown }).showErrorMessage = (
            _message: string,
        ): Thenable<string | undefined> => {
            errors++;
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/tmp/deactivation.qmd');
            const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
                runRender: async () => successfulRenderResult({
                    exitCode: null,
                    cancelled: true,
                }),
            }));

            await runRender(uri);
            assert.strictEqual(errors, 0);
        } finally {
            (vscode.window as { showErrorMessage: unknown }).showErrorMessage = original;
        }
    });

    function fakeDeps(overrides: Partial<QuartoCommandDeps> = {}): QuartoCommandDeps {
        return {
            resolver: { resolve: async () => 'quarto' },
            runtime: {
                registerPendingStart: (sourceFsPath) => ({ id: 1, sourceFsPath }),
                reconcilePendingStart: () => true,
                consumePendingStart: () => true,
                releasePendingStart: () => undefined,
                startOrRestart: async () => ({ kind: 'superseded', generation: 1 }),
                stopByLookup: async () => 'none',
            } as QuartoCommandDeps['runtime'],
            output,
            runRender: async () => ({
                exitCode: 0,
                stdout: '',
                stderr: '',
                cancelled: false,
                timedOut: false,
                spawnError: null,
            }),
            isWorkspaceTrusted: () => true,
            resolveContext: (sourceFsPath) => ({
                key: sourceFsPath,
                cwd: path.dirname(sourceFsPath),
                projectRoot: null,
            }),
            openTextDocument: async (uri) => fakeDocument(uri, '# document'),
            ...overrides,
        };
    }
});

function successfulRenderResult(
    overrides: Partial<KnitEngineResult> = {},
): KnitEngineResult {
    return {
        exitCode: 0,
        stdout: '',
        stderr: '',
        cancelled: false,
        timedOut: false,
        spawnError: null,
        ...overrides,
    };
}

function fakeDocument(uri: vscode.Uri, text: string): vscode.TextDocument {
    return {
        uri,
        isDirty: false,
        getText: () => text,
        save: async () => true,
    } as unknown as vscode.TextDocument;
}
