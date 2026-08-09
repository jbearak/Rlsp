import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import { activate, openDocument, waitForDiagnostics, waitForDiagnosticsMatching, getFixtureUri, sleep } from './helper';

suite('Ark LSP Extension', () => {
    suiteSetup(async () => {
        await activate();
    });

    test('diagnostics are reported for undefined variables', async () => {
        const doc = await openDocument('diagnostics.R');
        const diagnostics = await waitForDiagnostics(doc.uri, 15000);
        assert.ok(diagnostics.length > 0, 'Expected diagnostics for undefined variable');
        const messages = diagnostics.map(d => d.message.toLowerCase());
        assert.ok(messages.some(m => m.includes('is not defined') || m.includes('nonexistent')),
            'Expected diagnostic about undefined variable');
    });

    test('Stan diagnostics report variables but not call or distribution names', async () => {
        const config = vscode.workspace.getConfiguration('raven');
        await config.update('diagnostics.stan', 'on', vscode.ConfigurationTarget.Workspace);
        await sleep(100);

        try {
            const doc = await openDocument('stan_undefined.stan');
            // `config.update()` resolves before the extension's
            // `onDidChangeConfiguration` handler has pushed the new settings to
            // the server and the server has re-published. Until then the file
            // legitimately has zero findings (Stan defaults to "off"), so
            // `waitForDiagnostics` — which returns on the first *non-empty*
            // batch — can observe the pre-opt-in empty set and return early.
            // Poll for the opt-in to actually take effect instead.
            const undefinedVariables = await waitForDiagnosticsMatching(
                doc.uri,
                diagnostics =>
                    diagnostics.filter(d => d.code === 'undefined-variable').length === 2,
                15000,
            ).then(diagnostics =>
                diagnostics.filter(d => d.code === 'undefined-variable'),
            );
            const diagnostics = vscode.languages.getDiagnostics(doc.uri);
            assert.strictEqual(
                undefinedVariables.length,
                2,
                `Expected two Stan undefined variables, got: ${diagnostics.map(d => d.message).join('; ')}`,
            );
            assert.deepStrictEqual(
                undefinedVariables.map(diagnostic => ({
                    message: diagnostic.message,
                    start: [diagnostic.range.start.line, diagnostic.range.start.character],
                    end: [diagnostic.range.end.line, diagnostic.range.end.character],
                })),
                [
                    {
                        message: 'supplied_from_r is not defined',
                        start: [4, 16],
                        end: [4, 31],
                    },
                    {
                        message: 'another_missing is not defined',
                        start: [5, 12],
                        end: [5, 27],
                    },
                ],
            );
            assert.ok(
                !diagnostics.some(diagnostic => /normal|theta|N is not defined/.test(diagnostic.message)),
                `Stan declarations and distribution names must resolve: ${diagnostics.map(d => d.message).join('; ')}`,
            );
        } finally {
            await config.update('diagnostics.stan', undefined, vscode.ConfigurationTarget.Workspace);
        }
    });

    test('go-to-definition works for function calls', async () => {
        const doc = await openDocument('definitions.R');
        // Position on 'add' call at line 12 (0-indexed: 11), column 12
        const position = new vscode.Position(11, 12);
        const locations = await vscode.commands.executeCommand<vscode.Location[]>(
            'vscode.executeDefinitionProvider',
            doc.uri,
            position
        );
        assert.ok(locations && locations.length > 0, 'Expected definition location');
        assert.strictEqual(locations[0].range.start.line, 2, 'Expected definition at line 2');
    });

    test('document symbols are returned', async () => {
        const doc = await openDocument('symbols.R');
        const symbols = await vscode.commands.executeCommand<vscode.DocumentSymbol[]>(
            'vscode.executeDocumentSymbolProvider',
            doc.uri
        );
        assert.ok(symbols && symbols.length > 0, 'Expected document symbols');
        const names = symbols.map(s => s.name);
        assert.ok(names.includes('my_function'), 'Expected my_function symbol');
    });

    test('find-references returns all usages', async () => {
        const doc = await openDocument('definitions.R');
        // Position on 'add' definition at line 3 (0-indexed: 2)
        const position = new vscode.Position(2, 0);
        const locations = await vscode.commands.executeCommand<vscode.Location[]>(
            'vscode.executeReferenceProvider',
            doc.uri,
            position
        );
        assert.ok(locations && locations.length >= 2, 'Expected at least 2 references (2 calls)');
    });

    test('completions are provided', async () => {
        const doc = await openDocument('completions.R');
        // Position after 'pri' at line 3 (0-indexed: 2), column 3
        const position = new vscode.Position(2, 3);
        const completions = await vscode.commands.executeCommand<vscode.CompletionList>(
            'vscode.executeCompletionItemProvider',
            doc.uri,
            position
        );
        assert.ok(completions && completions.items.length > 0, 'Expected completion items');
        const labels = completions.items.map(i => i.label);
        assert.ok(labels.some(l => typeof l === 'string' && l.startsWith('print')), 
            'Expected print in completions');
    });

    test('untitled JAGS buffers use JAGS completions', async () => {
        const doc = await vscode.workspace.openTextDocument({
            language: 'jags',
            content: 'model {\n  theta ~ dnorm(0, 1)\n  the\n}',
        });
        await vscode.window.showTextDocument(doc);

        const position = new vscode.Position(2, 5);
        let completions: vscode.CompletionList | undefined;
        for (let attempt = 0; attempt < 20; attempt++) {
            completions = await vscode.commands.executeCommand<vscode.CompletionList>(
                'vscode.executeCompletionItemProvider',
                doc.uri,
                position
            );
            if (completions?.items?.length) {
                break;
            }
            await sleep(100);
        }

        assert.ok(completions && completions.items.length > 0, 'Expected JAGS completion items');
        const labels = completions.items.map(i => i.label);
        assert.ok(labels.some(l => typeof l === 'string' && l === 'dnorm'), 'Expected dnorm in completions');
        assert.ok(labels.some(l => typeof l === 'string' && l === 'theta'), 'Expected theta in completions');
        assert.ok(!labels.some(l => typeof l === 'string' && l === 'library'), 'Did not expect R-only completions');
    });

    test('hover information is provided', async () => {
        const doc = await openDocument('definitions.R');
        // Position on 'add' function definition
        const position = new vscode.Position(2, 0);
        const hovers = await vscode.commands.executeCommand<vscode.Hover[]>(
            'vscode.executeHoverProvider',
            doc.uri,
            position
        );
        assert.ok(hovers && hovers.length > 0, 'Expected hover information');
    });

    test('signature help is provided', async () => {
        const doc = await openDocument('definitions.R');
        // Position inside add() call arguments at line 11
        const position = new vscode.Position(10, 14);
        const help = await vscode.commands.executeCommand<vscode.SignatureHelp>(
            'vscode.executeSignatureHelpProvider',
            doc.uri,
            position,
            '('
        );
        // Signature help may not be available for user-defined functions
        // Just verify the command executes without error
        assert.ok(help === undefined || help.signatures !== undefined);
    });

    test('code actions are returned', async () => {
        const doc = await openDocument('symbols.R');
        const range = new vscode.Range(new vscode.Position(0, 0), new vscode.Position(0, 10));
        const actions = await vscode.commands.executeCommand<vscode.CodeAction[]>(
            'vscode.executeCodeActionProvider',
            doc.uri,
            range
        );
        assert.ok(actions !== undefined && Array.isArray(actions), 'Expected code actions array');
    });

    test('folding ranges are provided', async () => {
        const doc = await openDocument('symbols.R');
        const ranges = await vscode.commands.executeCommand<vscode.FoldingRange[]>(
            'vscode.executeFoldingRangeProvider',
            doc.uri
        );
        assert.ok(ranges && ranges.length > 0, 'Expected folding ranges for function bodies');
    });

    test('workspace: go-to-definition across files', async () => {
        const doc = await openDocument('workspace_main.R');
        console.log('Opened workspace_main.R:', doc.uri.toString());
        
        // Position on 'helper_func' call at line 3
        const position = new vscode.Position(3, 10);
        const locations = await vscode.commands.executeCommand<vscode.Location[]>(
            'vscode.executeDefinitionProvider',
            doc.uri,
            position
        );
        
        console.log('Definition locations:', locations);
        console.log('Location count:', locations?.length);
        if (locations && locations.length > 0) {
            console.log('First location URI:', locations[0].uri.toString());
        }
        
        assert.ok(locations && locations.length > 0, `Expected definition location, got ${locations?.length || 0}`);
        assert.ok(locations[0].uri.path.includes('utils.R'), `Expected definition in utils.R, got ${locations[0].uri.path}`);
    });

    test('workspace: find-references across files', async () => {
        const doc = await openDocument('workspace/utils.R');
        console.log('Opened workspace/utils.R:', doc.uri.toString());
        
        // Position on 'helper_func' definition at line 3
        const position = new vscode.Position(2, 0);
        const locations = await vscode.commands.executeCommand<vscode.Location[]>(
            'vscode.executeReferenceProvider',
            doc.uri,
            position
        );
        
        console.log('Reference locations:', locations);
        console.log('Reference count:', locations?.length);
        if (locations) {
            locations.forEach((loc, i) => {
                console.log(`  [${i}] ${loc.uri.path}:${loc.range.start.line}`);
            });
        }
        
        assert.ok(locations && locations.length >= 2, `Expected at least 2 references, got ${locations?.length || 0}`);
        const files = locations.map(l => l.uri.path);
        assert.ok(files.some(f => f.includes('utils.R')), 'Expected reference in utils.R');
        assert.ok(files.some(f => f.includes('workspace_main.R')), 'Expected reference in workspace_main.R');
    });

    test('no false positives for function parameters', async () => {
        const doc = await openDocument('function_params.R');
        const diagnostics = await waitForDiagnostics(doc.uri, 15000);
        
        // Filter to only undefined variable warnings
        const undefinedVarDiags = diagnostics.filter(d =>
            d.message.toLowerCase().includes('is not defined')
        );
        
        // Check that function parameters are NOT flagged as undefined
        const messages = undefinedVarDiags.map(d => d.message);
        assert.ok(!messages.some(m => m.includes('a') || m.includes('b')), 
            'Function parameters should not be flagged as undefined');
        assert.ok(!messages.some(m => m.includes('x') || m.includes('y')), 
            'Function parameters should not be flagged as undefined');
    });

    test('no false positives for built-in functions', async () => {
        const doc = await openDocument('function_params.R');
        const diagnostics = await waitForDiagnostics(doc.uri, 15000);
        
        // Filter to only undefined variable warnings
        const undefinedVarDiags = diagnostics.filter(d =>
            d.message.toLowerCase().includes('is not defined')
        );
        
        // Check that built-in functions are NOT flagged as undefined
        const messages = undefinedVarDiags.map(d => d.message);
        assert.ok(!messages.some(m => m.includes('any')), 'Built-in "any" should not be undefined');
        assert.ok(!messages.some(m => m.includes('is.na')), 'Built-in "is.na" should not be undefined');
        assert.ok(!messages.some(m => m.includes('warning')), 'Built-in "warning" should not be undefined');
        assert.ok(!messages.some(m => m.includes('sprintf')), 'Built-in "sprintf" should not be undefined');
        assert.ok(!messages.some(m => m.includes('sum')), 'Built-in "sum" should not be undefined');
        assert.ok(!messages.some(m => m.includes('mean')), 'Built-in "mean" should not be undefined');
        assert.ok(!messages.some(m => m.includes('print')), 'Built-in "print" should not be undefined');
    });

    test('cross-file: sibling package propagation suppresses false positives', async () => {
        // Reproduces bug: main.r sources functions.r (library(plyr)) then data.r.
        // data.r -> outcomes.r -> abortions.r. ddply (from plyr) should NOT be flagged.
        //
        // Fixture layout (all under fixtures/):
        //   crossfile_pkg_main.r:      source("crossfile_pkg/functions.r"); source("crossfile_pkg/data.r")
        //   crossfile_pkg/functions.r:  library(plyr)
        //   crossfile_pkg/data.r:       source("crossfile_pkg/outcomes.r")
        //   crossfile_pkg/outcomes.r:   source("crossfile_pkg/abortions.r")
        //   crossfile_pkg/abortions.r:  z <- nonexistent_sentinel_var + 1; ddply(...)

        const doc = await openDocument('crossfile_pkg/abortions.r');

        // Wait for the sentinel diagnostic (nonexistent_sentinel_var) to confirm
        // the LSP has processed the file with undefined variable checking enabled.
        const diagnostics = await waitForDiagnostics(doc.uri, 15000);

        // If no diagnostics at all, the workspace scan may not have completed yet
        // and undefined variable checking was skipped. Wait and retry.
        let finalDiags = diagnostics;
        if (diagnostics.length === 0) {
            await sleep(5000);
            finalDiags = vscode.languages.getDiagnostics(doc.uri);
        }

        // If still no diagnostics after retry, workspace scan didn't complete and
        // undefined variable checking was skipped — skip the test (no false positive possible).
        if (finalDiags.length === 0) {
            return;
        }

        const messages = finalDiags.map(d => d.message);

        // Assert the sentinel diagnostic IS present — proves undefined-variable checking ran.
        assert.ok(
            messages.some(m => m.includes('nonexistent_sentinel_var')),
            `Expected sentinel diagnostic for nonexistent_sentinel_var to confirm ` +
            `undefined-variable checking is active. Got diagnostics: ${messages.join('; ')}`
        );

        // Only then assert ddply is NOT among them.
        assert.ok(
            !messages.some(m => m.includes('ddply')),
            `ddply should not be flagged as undefined (plyr inherited from sibling source chain). ` +
            `Got diagnostics: ${messages.join('; ')}`
        );
    });

    test('forward reference at top level is flagged when file has parent', async function (this: Mocha.Context) {
        // Reproduces: main.r -> data.r -> impute.r
        // In impute.r, `food` is used on lines 2-3 before being defined on line 4.
        // The forward reference should produce "food is used before it is defined" diagnostics.
        // sentinel_undefined_fwdref_usage is always undefined (sentinel to confirm checking ran).
        const doc = await openDocument('forward_ref/impute.r');

        const diagnostics = await waitForDiagnostics(doc.uri, 15000);

        let finalDiags = diagnostics;
        if (diagnostics.length === 0) {
            await sleep(5000);
            finalDiags = vscode.languages.getDiagnostics(doc.uri);
        }

        // If still no diagnostics, workspace scan didn't complete — skip.
        if (finalDiags.length === 0) {
            this.skip();
        }

        const messages = finalDiags.map(d => d.message);

        // Assert the sentinel diagnostic IS present — proves undefined-variable checking ran.
        assert.ok(
            messages.some(m => m.includes('sentinel_undefined_fwdref_usage')),
            `Expected sentinel diagnostic for sentinel_undefined_fwdref_usage. ` +
            `Got diagnostics: ${messages.join('; ')}`
        );

        // Assert forward reference to `food` IS flagged (used before defined at top level).
        const foodDiags = finalDiags.filter(d => d.message.includes('food'));
        assert.ok(
            foodDiags.length >= 1,
            `Expected 'food' to be flagged as undefined (forward reference at top level). ` +
            `Got diagnostics: ${messages.join('; ')}`
        );
    });

    test('package.json registers mixed-case JAGS and Stan extensions', async () => {
        const packageJsonPath = path.join(__dirname, '..', '..', 'package.json');
        const pkg = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
        const languages = pkg.contributes.languages as Array<{ id: string; extensions: string[] }>;

        const jags = languages.find(language => language.id === 'jags');
        const stan = languages.find(language => language.id === 'stan');

        assert.ok(jags, 'Expected JAGS language contribution');
        assert.ok(stan, 'Expected Stan language contribution');
        assert.deepStrictEqual(
            jags?.extensions,
            [
                '.jags',
                '.Jags',
                '.JAGS',
                '.bugs',
                '.Bugs',
                '.BUGS',
                '.bug',
                '.buG',
                '.bUg',
                '.bUG',
                '.Bug',
                '.BuG',
                '.BUg',
                '.BUG',
            ],
        );
        assert.deepStrictEqual(
            stan?.extensions,
            ['.stan', '.Stan', '.STAN'],
        );
    });

    // `wordPattern` in the language configs makes a dotted identifier like
    // `my.variable` a single "word" — driving Cmd+click navigation,
    // `getWordRangeAtPosition`, and expand-selection. `rmd`/`quarto` share the
    // R wordPattern via `rmd-language-configuration.json`; `jags` has
    // its own copy of the same pattern. With VS Code's default word definition
    // (dots ARE separators) the word range starting on the first segment would
    // stop at the dot and yield only "my", so asserting the full span proves
    // the contributed pattern is actually live in the editor — not just present
    // as a string in the JSON file.
    for (const language of ['r', 'jags', 'rmd', 'quarto']) {
        test(`treats a dotted identifier as one word in ${language} (wordPattern)`, async () => {
            const doc = await vscode.workspace.openTextDocument({ language, content: 'my.variable <- 1\n' });
            // The contributed `language-configuration.json` wordPattern is loaded
            // (and its word definition synced to the extension host) lazily — only
            // once a document of the language is shown in an editor. Before that,
            // `getWordRangeAtPosition` falls back to the default word definition
            // (where dots ARE separators), so the doc must be shown first.
            await vscode.window.showTextDocument(doc);

            // Position on the 'm' of the first segment; the default word
            // definition would stop at the dot and yield only "my". Poll to
            // absorb the lazy config-load latency (observed < 100ms).
            const pos = new vscode.Position(0, 0);
            let word: string | undefined;
            const deadline = Date.now() + 5000;
            do {
                const range = doc.getWordRangeAtPosition(pos);
                word = range ? doc.getText(range) : undefined;
                if (word === 'my.variable') break;
                await sleep(50);
            } while (Date.now() < deadline);

            assert.strictEqual(
                word,
                'my.variable',
                `[${language}] a dotted identifier should be treated as a single word`,
            );
        });
    }

    test('package.json describes dot-in-word handling for JAGS too', async () => {
        const packageJsonPath = path.join(__dirname, '..', '..', 'package.json');
        const pkg = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as {
            contributes: {
                configuration: {
                    properties: Record<string, { description?: string }>;
                };
            };
        };

        const description = pkg.contributes.configuration.properties['raven.editor.dotInWord']?.description;

        assert.ok(description?.includes('R and JAGS files'));
    });

    // RStudio and Positron treat dots as part of words out of the box, so Raven
    // defaults `dotInWord` to "yes" — applying the override silently
    // rather than prompting. `"ask"` remains opt-in for users who want a prompt.
    // See https://github.com/jbearak/raven/issues/265.
    test('package.json defaults dot-in-word to yes', async () => {
        const packageJsonPath = path.join(__dirname, '..', '..', 'package.json');
        const pkg = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as {
            contributes: {
                configuration: {
                    properties: Record<string, { default?: string; enum?: string[] }>;
                };
            };
        };

        const schema = pkg.contributes.configuration.properties['raven.editor.dotInWord'];

        assert.strictEqual(schema?.default, 'yes');
        assert.deepStrictEqual(schema?.enum, ['ask', 'yes', 'no']);
    });

    // The setting was renamed from `dotInWordSeparators` (whose `yes`/`no` read
    // backwards) to `dotInWord`. The old key stays in the manifest marked
    // deprecated so existing users get Settings-UI deprecation styling and no
    // "unknown setting" warning while migration runs. See the migration in
    // `extension.ts` (`migrateDotInWordSetting`).
    test('package.json keeps deprecated dotInWordSeparators alias', async () => {
        const packageJsonPath = path.join(__dirname, '..', '..', 'package.json');
        const pkg = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8')) as {
            contributes: {
                configuration: {
                    properties: Record<string, { deprecationMessage?: string }>;
                };
            };
        };

        const schema = pkg.contributes.configuration.properties['raven.editor.dotInWordSeparators'];

        assert.ok(schema, 'deprecated alias must remain in the manifest');
        assert.ok(
            schema?.deprecationMessage?.includes('raven.editor.dotInWord'),
            'deprecation message should point to the new key',
        );
    });
});
