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
    let trueShowInformationMessage: typeof vscode.window.showInformationMessage;
    let trueShowErrorMessage: typeof vscode.window.showErrorMessage;

    suiteSetup(async () => {
        await activate();
        output = vscode.window.createOutputChannel('Quarto Command Test');
    });

    suiteTeardown(() => output.dispose());

    setup(() => {
        trueShowInformationMessage = vscode.window.showInformationMessage;
        trueShowErrorMessage = vscode.window.showErrorMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            _message: string,
        ): Thenable<string | undefined> => Promise.resolve(undefined);
        (vscode.window as { showErrorMessage: unknown }).showErrorMessage = (
            _message: string,
        ): Thenable<string | undefined> => Promise.resolve(undefined);
    });

    teardown(() => {
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            trueShowInformationMessage
        );
        (vscode.window as { showErrorMessage: unknown }).showErrorMessage = (
            trueShowErrorMessage
        );
    });

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

    test('closed R-console gate blocks Preview before trust or document work', async () => {
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
                vscode.Uri.file('/tmp/gated-preview.qmd'),
                'Preview',
                fakeDeps({
                    resolveRConsoleActivation: () => 'disabled',
                    isWorkspaceTrusted: () => { throw new Error('trust must not run'); },
                    openTextDocument: async () => { throw new Error('open must not run'); },
                }),
            );

            assert.strictEqual(result, null);
            assert.deepStrictEqual(messages, [
                'Raven: Quarto Preview is disabled by your ' +
                '`raven.rConsole.activation` setting (or because REditorSupport / ' +
                'Positron is active).',
            ]);
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('closed R-console gate blocks Render before project discovery', async () => {
        let contextResolved = false;
        let rendered = false;
        const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
            resolveRConsoleActivation: () => 'disabled',
            resolveContext: async () => {
                contextResolved = true;
                throw new Error('context must not resolve');
            },
            runRender: async () => {
                rendered = true;
                throw new Error('render must not run');
            },
        }));

        await runRender(vscode.Uri.file('/tmp/gated-render.qmd'));

        assert.strictEqual(contextResolved, false);
        assert.strictEqual(rendered, false);
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

    test('dirty document is saved before the render subprocess reads it', async () => {
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

        let renderSawSavedMarker = false;
        try {
            const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
                resolveContext: async () => ({
                    key: sourcePath,
                    cwd: tmp,
                    projectRoot: null,
                }),
                openTextDocument: async (requestedUri) => {
                    assert.strictEqual(requestedUri.toString(), uri.toString());
                    return doc;
                },
                runRender: async () => {
                    renderSawSavedMarker = fs.readFileSync(sourcePath, 'utf8')
                        .includes('Saved marker');
                    return successfulRenderResult();
                },
            }));

            await runRender(uri);

            assert.strictEqual(renderSawSavedMarker, true);
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
                    resolveRConsoleActivation: () => {
                        throw new Error('R-console gate must not run');
                    },
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
                    resolveContext: async (sourceFsPath) => ({
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
                const wasPending = pendingActive;
                pendingActive = false;
                // A pending-only cancel is 'cancelled-pending', never 'stopped'.
                return wasPending ? 'cancelled-pending' as const : 'none' as const;
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
                resolveContext: async () => ({
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

    test('Stop cancels a pending intent and still stops a running project preview', async () => {
        // Regression: cancelling a source-level pending intent used to short-
        // circuit the project-key fallback, so Stop reported success while a
        // preview owned under the project key kept running. Stop must consult
        // the project key whenever no session was stopped by the lexical key.
        const messages: string[] = [];
        const lookups: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/chapter.qmd');
            const deps = fakeDeps({
                resolveContext: async () => ({
                    key: '/project',
                    cwd: '/project',
                    projectRoot: '/project',
                }),
                runtime: {
                    registerPendingStart: () => { throw new Error('register must not run'); },
                    reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                    consumePendingStart: () => { throw new Error('consume must not run'); },
                    releasePendingStart: () => { throw new Error('release must not run'); },
                    startOrRestart: async () => { throw new Error('start must not run'); },
                    stopByLookup: async (key) => {
                        lookups.push(key);
                        // Lexical key: only a preflight intent is cancelled.
                        if (key === '/project/chapter.qmd') return 'cancelled-pending';
                        // Project key: the actually-running project preview.
                        if (key === '/project') return 'stopped';
                        return 'none';
                    },
                } as QuartoCommandDeps['runtime'],
            });
            await runQuartoStopForTesting(uri, deps);

            assert.deepStrictEqual(lookups, ['/project/chapter.qmd', '/project']);
            assert.ok(messages.includes('Quarto preview stopped.'));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('Stop that stops a session still records the project key while an intent is pending', async () => {
        // Regression: an alias-resolved Stop that succeeds under a stale session
        // key used to skip the project-key phase, so a sibling pending Preview
        // for the current project (its key changed since the session started)
        // was never abandoned and relaunched after Stop. With an intent pending,
        // the project key must be consulted even on a 'stopped' lexical result.
        const lookups: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            void message;
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/a.qmd');
            const deps = fakeDeps({
                resolveContext: async () => ({
                    key: '/project',
                    cwd: '/project',
                    projectRoot: '/project',
                }),
                runtime: {
                    registerPendingStart: () => { throw new Error('register must not run'); },
                    reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                    consumePendingStart: () => { throw new Error('consume must not run'); },
                    releasePendingStart: () => { throw new Error('release must not run'); },
                    startOrRestart: async () => { throw new Error('start must not run'); },
                    beginStop: () => 7,
                    hasPendingStarts: () => true,
                    stopByLookup: async (key) => {
                        lookups.push(key);
                        return key === '/project/a.qmd' ? 'stopped' : 'none';
                    },
                } as QuartoCommandDeps['runtime'],
            });
            await runQuartoStopForTesting(uri, deps);

            assert.deepStrictEqual(lookups, ['/project/a.qmd', '/project']);
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('Stop confirms a cancelled intent even when project discovery fails', async () => {
        // Regression: routing a pending-cancel through the project-key phase must
        // not let a failing/hung project discovery swallow the confirmation for
        // work already done.
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/a.qmd');
            const deps = fakeDeps({
                resolveContext: async () => { throw new Error('remote filesystem hung'); },
                runtime: {
                    registerPendingStart: () => { throw new Error('register must not run'); },
                    reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                    consumePendingStart: () => { throw new Error('consume must not run'); },
                    releasePendingStart: () => { throw new Error('release must not run'); },
                    startOrRestart: async () => { throw new Error('start must not run'); },
                    beginStop: () => 3,
                    hasPendingStarts: () => false,
                    stopByLookup: async () => 'cancelled-pending',
                } as QuartoCommandDeps['runtime'],
            });

            // Must resolve (not reject) and still confirm the cancellation.
            await runQuartoStopForTesting(uri, deps);
            assert.ok(messages.includes('Quarto preview stopped.'));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('a second Stop during teardown stays silent, not "no preview running"', async () => {
        // Regression: a lexical 'already-stopping' (a prior Stop still draining)
        // must not be downgraded to 'none' by a project-key fallback that finds
        // nothing, which would show a misleading "No Quarto preview is running".
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/a.qmd');
            const deps = fakeDeps({
                resolveContext: async () => ({
                    key: '/project',
                    cwd: '/project',
                    projectRoot: '/project',
                }),
                runtime: {
                    registerPendingStart: () => { throw new Error('register must not run'); },
                    reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                    consumePendingStart: () => { throw new Error('consume must not run'); },
                    releasePendingStart: () => { throw new Error('release must not run'); },
                    startOrRestart: async () => { throw new Error('start must not run'); },
                    beginStop: () => 5,
                    hasPendingStarts: () => false,
                    stopByLookup: async (key) =>
                        key === '/project/a.qmd' ? 'already-stopping' : 'none',
                } as QuartoCommandDeps['runtime'],
            });
            await runQuartoStopForTesting(uri, deps);

            assert.ok(
                !messages.includes('No Quarto preview is running for this document.'),
                'already-stopping must not surface a "no preview" message',
            );
            assert.ok(!messages.includes('Quarto preview stopped.'));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('Stop stays bounded and confirms a cancel when project discovery hangs', async () => {
        // Regression: a wedged remote filesystem could hang project discovery
        // forever, leaving Stop pending and holding deactivation. Discovery is
        // now bounded; on timeout the completed pending-cancel still confirms.
        const messages: string[] = [];
        const original = vscode.window.showInformationMessage;
        (vscode.window as { showInformationMessage: unknown }).showInformationMessage = (
            message: string,
        ): Thenable<string | undefined> => {
            messages.push(message);
            return Promise.resolve(undefined);
        };
        try {
            const uri = vscode.Uri.file('/project/a.qmd');
            const deps = fakeDeps({
                contextTimeoutMs: 20,
                resolveContext: () => new Promise<never>(() => { /* never settles */ }),
                runtime: {
                    registerPendingStart: () => { throw new Error('register must not run'); },
                    reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
                    consumePendingStart: () => { throw new Error('consume must not run'); },
                    releasePendingStart: () => { throw new Error('release must not run'); },
                    startOrRestart: async () => { throw new Error('start must not run'); },
                    beginStop: () => 4,
                    hasPendingStarts: () => false,
                    stopByLookup: async () => 'cancelled-pending',
                } as QuartoCommandDeps['runtime'],
            });

            // Must resolve (bounded), not hang, and still confirm the cancel.
            await runQuartoStopForTesting(uri, deps);
            assert.ok(messages.includes('Quarto preview stopped.'));
        } finally {
            (vscode.window as { showInformationMessage: unknown }).showInformationMessage = original;
        }
    });

    test('a Preview whose project discovery hangs releases its pending intent', async () => {
        // Regression: an indefinitely-hung Preview preflight would pin a pending
        // intent forever, which (a) never launches and (b) keeps the runtime's
        // stop-epoch map from ever pruning. Bounded discovery makes the hang a
        // rejection, and the finally releases the intent.
        let registered = 0;
        let released = 0;
        const runtime = {
            registerPendingStart: (sourceFsPath: string) => {
                registered++;
                return { id: 1, sourceFsPath };
            },
            reconcilePendingStart: () => { throw new Error('reconcile must not run'); },
            consumePendingStart: () => { throw new Error('consume must not run'); },
            releasePendingStart: () => { released++; },
            startOrRestart: async () => { throw new Error('start must not run'); },
            stopByLookup: async () => 'none' as const,
        };
        const deps = fakeDeps({
            contextTimeoutMs: 20,
            resolveContext: () => new Promise<never>(() => { /* never settles */ }),
            runtime: runtime as unknown as QuartoCommandDeps['runtime'],
        });
        const runPreview = createQuartoPreviewRunnerForTesting(deps);

        // The hung discovery rejects via the bound; the Preview must not leak
        // its intent.
        await runPreview(vscode.Uri.file('/project/a.qmd')).catch(() => undefined);
        assert.strictEqual(registered, 1);
        assert.strictEqual(released, 1);
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

    test('physical source aliases share one standalone render guard', async () => {
        const renderStarted = new Deferred<void>();
        const finishRender = new Deferred<KnitEngineResult>();
        let renders = 0;
        const real = vscode.Uri.file('/project/real/doc.qmd');
        const alias = vscode.Uri.file('/project/link/doc.qmd');
        const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
            resolveContext: async () => ({
                key: real.fsPath,
                cwd: path.dirname(real.fsPath),
                projectRoot: null,
            }),
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
            resolveContext: async () => ({
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
        const base = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-quarto-guard-'));
        try {
            const projectRoot = path.join(base, 'project');
            const realSource = path.join(projectRoot, 'chapters', 'doc.qmd');
            const aliasSource = path.join(base, 'outside.qmd');
            fs.mkdirSync(path.dirname(realSource), { recursive: true });
            fs.writeFileSync(path.join(projectRoot, '_quarto.yml'), 'project:\n  type: default\n');
            fs.writeFileSync(realSource, '# document\n');
            fs.symlinkSync(realSource, aliasSource);
            const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
                resolveContext: undefined,
                runRender: async () => {
                    renders++;
                    renderStarted.resolve(undefined);
                    return finishRender.promise;
                },
            }));

            const first = runRender(vscode.Uri.file(realSource));
            await renderStarted.promise;
            await runRender(vscode.Uri.file(aliasSource));

            assert.strictEqual(renders, 1);
            finishRender.resolve(successfulRenderResult());
            await first;
        } finally {
            finishRender.resolve(successfulRenderResult());
            fs.rmSync(base, { recursive: true, force: true });
        }
    });

    test('different projects and standalone files render concurrently', async () => {
        const runPair = async (projectRoots: boolean): Promise<void> => {
            const bothStarted = new Deferred<void>();
            const finishRender = new Deferred<KnitEngineResult>();
            let renders = 0;
            const runRender = createQuartoRenderRunnerForTesting(fakeDeps({
                resolveContext: async (sourceFsPath) => {
                    const parent = path.dirname(sourceFsPath);
                    return {
                        key: projectRoots ? parent : sourceFsPath,
                        cwd: parent,
                        projectRoot: projectRoots ? parent : null,
                    };
                },
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
            resolveContext: async (sourceFsPath) => ({
                key: sourceFsPath,
                cwd: path.dirname(sourceFsPath),
                projectRoot: null,
            }),
            resolveRConsoleActivation: () => 'enabled',
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
