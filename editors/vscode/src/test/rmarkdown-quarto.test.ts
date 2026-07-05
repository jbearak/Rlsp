/**
 * Integration tests for R Markdown / Quarto chunk detection using demo files.
 *
 * Opens the .Rmd, .Rmarkdown, and .qmd files from demo/rmarkdown-quarto/ and verifies
 * that Raven detects R code chunks (via CodeLens or document symbols).
 */

import * as assert from 'assert';
import * as path from 'path';
import * as vscode from 'vscode';
import { activate, sleep } from './helper';

function demoPath(file: string): string {
    return path.resolve(__dirname, '..', '..', '..', '..', 'demo', 'rmarkdown-quarto', file);
}

async function openAndGetCodeLenses(
    filePath: string,
    timeoutMs = 15000,
): Promise<{ lenses: vscode.CodeLens[]; languageId: string }> {
    const uri = vscode.Uri.file(filePath);
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);

    // Poll for CodeLens to appear (chunk detection is async).
    const deadline = Date.now() + timeoutMs;
    let lenses: vscode.CodeLens[] = [];
    while (Date.now() < deadline) {
        await sleep(500);
        lenses = (await vscode.commands.executeCommand<vscode.CodeLens[]>(
            'vscode.executeCodeLensProvider',
            uri,
        )) ?? [];
        if (lenses.length > 0) break;
    }
    return { lenses, languageId: doc.languageId };
}

suite('rmarkdown-quarto chunk detection', function (this: Mocha.Suite) {
    this.timeout(60000);

    suiteSetup(async () => {
        await activate();
    });

    suiteTeardown(async () => {
        await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    });

    test('.Rmd file: R chunks are detected', async function () {
        // Skip if chunk commands are not registered (coexistence mode).
        const all = new Set(await vscode.commands.getCommands(true));
        if (!all.has('raven.runCurrentChunk')) {
            this.skip();
            return;
        }

        const { lenses, languageId } = await openAndGetCodeLenses(demoPath('analysis.Rmd'));
        // Pin the language id so the `files.associations` configurationDefaults
        // entry (or Raven's `contributes.languages` claim) can't silently drift
        // back to a state where `.Rmd` resolves to something other than `rmd`.
        // The Quarto extension is not present in the vscode-test host, so this
        // is a baseline-not-regressed check rather than the full reproduction
        // of the Quarto-installed bug, but it still catches an accidental
        // removal of either contribution.
        assert.strictEqual(
            languageId,
            'rmd',
            `Expected .Rmd file to resolve to languageId 'rmd', got '${languageId}'`,
        );
        assert.ok(
            lenses.length >= 2,
            `Expected at least 2 CodeLens entries for .Rmd chunks; got ${lenses.length}`,
        );
    });

    test('.Rmarkdown file: R chunks are detected', async function () {
        // Skip if chunk commands are not registered (coexistence mode).
        const all = new Set(await vscode.commands.getCommands(true));
        if (!all.has('raven.runCurrentChunk')) {
            this.skip();
            return;
        }

        const { lenses, languageId } = await openAndGetCodeLenses(demoPath('analysis.Rmarkdown'));
        assert.strictEqual(
            languageId,
            'rmd',
            `Expected .Rmarkdown file to resolve to languageId 'rmd', got '${languageId}'`,
        );
        assert.ok(
            lenses.length >= 2,
            `Expected at least 2 CodeLens entries for .Rmarkdown chunks; got ${lenses.length}`,
        );
    });

    test('.qmd file: R chunks are detected', async function () {
        const all = new Set(await vscode.commands.getCommands(true));
        if (!all.has('raven.runCurrentChunk')) {
            this.skip();
            return;
        }

        const { lenses } = await openAndGetCodeLenses(demoPath('report.qmd'));
        assert.ok(
            lenses.length >= 2,
            `Expected at least 2 CodeLens entries for .qmd chunks; got ${lenses.length}`,
        );
    });
});
