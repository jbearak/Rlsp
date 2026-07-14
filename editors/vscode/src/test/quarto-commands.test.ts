import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';
import { activate, awaitActive } from './helper';
import {
    runQuartoPreflightForTesting,
    runQuartoStopForTesting,
    type QuartoCommandDeps,
} from '../quarto/quarto-commands';

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

    function fakeDeps(overrides: Partial<QuartoCommandDeps> = {}): QuartoCommandDeps {
        return {
            resolver: { resolve: async () => 'quarto' },
            runtime: {
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
            ...overrides,
        };
    }
});

function fakeDocument(uri: vscode.Uri, text: string): vscode.TextDocument {
    return {
        uri,
        isDirty: false,
        getText: () => text,
        save: async () => true,
    } as unknown as vscode.TextDocument;
}
