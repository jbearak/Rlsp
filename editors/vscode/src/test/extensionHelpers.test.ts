import * as assert from 'assert';
import * as vscode from 'vscode';
import {
    clearIneligibleDiagnostics,
    diagnosticResourceUris,
    getUpdatedGlobalLanguageConfig,
    isRDocument,
    planDotInWordMigration,
    resolveTabSizeForDocument,
} from '../extensionHelpers';

suite('Extension Helpers', () => {
    test('diagnosticResourceUris includes tabs, modified diff sides, and peek editors', () => {
        const tabbed = vscode.Uri.file('/tmp/tabbed.R');
        const original = vscode.Uri.file('/tmp/original.R');
        const modified = vscode.Uri.file('/tmp/modified.R');
        const peeked = vscode.Uri.file('/tmp/peeked.R');

        const result = diagnosticResourceUris(
            [{
                tabs: [
                    { input: { uri: tabbed } },
                    { input: { original, modified }, isActive: true },
                    { input: { uri: tabbed } },
                    { input: { viewType: 'terminal' } },
                ],
            }],
            [
                { document: { uri: original } },
                { document: { uri: modified } },
                { document: { uri: peeked } },
            ],
        );

        assert.deepStrictEqual(result, [
            tabbed.toString(),
            modified.toString(),
            peeked.toString(),
        ]);
    });

    test('diagnosticResourceUris keeps a diff original that has its own tab', () => {
        const original = vscode.Uri.file('/tmp/original-with-tab.R');
        const modified = vscode.Uri.file('/tmp/modified-with-tab.R');

        const result = diagnosticResourceUris(
            [{
                tabs: [
                    { input: { original, modified } },
                    { input: { uri: original } },
                ],
            }],
            [
                { document: { uri: original } },
                { document: { uri: modified } },
            ],
        );

        assert.deepStrictEqual(result, [modified.toString(), original.toString()]);
    });

    test('diagnosticResourceUris keeps a peeked file matching an inactive diff original', () => {
        // An inactive diff renders no editors, so its original side cannot be
        // the source of a visible editor; a matching visible editor must come
        // from an independent element (e.g. a peek editor) and must count.
        const original = vscode.Uri.file('/tmp/inactive-diff-original.R');
        const modified = vscode.Uri.file('/tmp/inactive-diff-modified.R');
        const activeTab = vscode.Uri.file('/tmp/active-tab.R');

        const result = diagnosticResourceUris(
            [{
                tabs: [
                    { input: { original, modified }, isActive: false },
                    { input: { uri: activeTab }, isActive: true },
                ],
            }],
            [
                { document: { uri: activeTab } },
                { document: { uri: original } },
            ],
        );

        assert.deepStrictEqual(result, [
            modified.toString(),
            activeTab.toString(),
            original.toString(),
        ]);
    });

    test('clearIneligibleDiagnostics prunes only retained background resources', () => {
        const eligible = vscode.Uri.file('/tmp/eligible.R');
        const hidden = vscode.Uri.file('/tmp/hidden.R');
        const collection = vscode.languages.createDiagnosticCollection(
            'raven-diagnostic-ownership-test',
        );
        const marker = new vscode.Diagnostic(new vscode.Range(0, 0, 0, 1), 'marker');

        try {
            collection.set(eligible, [marker]);
            collection.set(hidden, [marker]);

            clearIneligibleDiagnostics(collection, [eligible.toString()]);

            assert.strictEqual(collection.get(eligible)?.length, 1);
            assert.strictEqual((collection.get(hidden) ?? []).length, 0);
        } finally {
            collection.dispose();
        }
    });

    test('isRDocument accepts untitled R-like documents by language id', () => {
        const makeUntitledDocument = (
            languageId: string,
        ): Pick<vscode.TextDocument, 'isUntitled' | 'languageId' | 'uri'> => ({
            isUntitled: true,
            languageId,
            uri: vscode.Uri.parse(`untitled:${languageId}`),
        });

        assert.strictEqual(isRDocument(makeUntitledDocument('r')), true);
        assert.strictEqual(isRDocument(makeUntitledDocument('jags')), true);
        assert.strictEqual(isRDocument(makeUntitledDocument('stan')), true);
        // R Markdown and Quarto are tracked under their own language IDs but
        // the LSP server does not parse them, so they intentionally do NOT
        // count as "R documents" for activity-tracking / path-completion-
        // trigger purposes.
        assert.strictEqual(isRDocument(makeUntitledDocument('rmd')), false);
        assert.strictEqual(isRDocument(makeUntitledDocument('quarto')), false);
        assert.strictEqual(isRDocument(makeUntitledDocument('plaintext')), false);
    });

    test('isRDocument accepts supported file-backed extensions', () => {
        const makeFileDocument = (
            filePath: string,
        ): Pick<vscode.TextDocument, 'isUntitled' | 'languageId' | 'uri'> => ({
            isUntitled: false,
            languageId: 'plaintext',
            uri: vscode.Uri.file(filePath),
        });

        assert.strictEqual(isRDocument(makeFileDocument('/tmp/script.R')), true);
        assert.strictEqual(isRDocument(makeFileDocument('/tmp/model.BUGS')), true);
        assert.strictEqual(isRDocument(makeFileDocument('/tmp/model.StAn')), true);
        // `.Rmd` and `.qmd` register under the dedicated `rmd` / `quarto`
        // languages and are not LSP-tracked.
        assert.strictEqual(isRDocument(makeFileDocument('/tmp/report.Rmd')), false);
        assert.strictEqual(isRDocument(makeFileDocument('/tmp/report.qmd')), false);
        assert.strictEqual(isRDocument(makeFileDocument('/tmp/notes.txt')), false);
    });

    test('getUpdatedGlobalLanguageConfig creates a global override when missing', () => {
        assert.deepStrictEqual(
            getUpdatedGlobalLanguageConfig(undefined, 'abc'),
            { 'editor.wordSeparators': 'abc' },
        );
    });

    test('getUpdatedGlobalLanguageConfig preserves unrelated global keys', () => {
        assert.deepStrictEqual(
            getUpdatedGlobalLanguageConfig(
                {
                    globalValue: {
                        'editor.tabSize': 2,
                    },
                },
                'abc',
            ),
            {
                'editor.tabSize': 2,
                'editor.wordSeparators': 'abc',
            },
        );
    });

    test('getUpdatedGlobalLanguageConfig returns null when already correct globally', () => {
        assert.strictEqual(
            getUpdatedGlobalLanguageConfig(
                {
                    globalValue: {
                        'editor.wordSeparators': 'abc',
                    },
                },
                'abc',
            ),
            null,
        );
    });

    test('resolveTabSizeForDocument passes language-scoped configuration scope', () => {
        // The scope passed to getConfiguration must include `languageId` so
        // VS Code resolves [r]-scoped overrides like `[r] { "editor.tabSize": 2 }`.
        // A bare vscode.Uri scope only reads resource-scoped configuration and
        // misses language-specific overrides.
        const doc = {
            uri: vscode.Uri.file('/proj/foo.R'),
            languageId: 'r',
        };

        let capturedScope: vscode.ConfigurationScope | undefined;
        resolveTabSizeForDocument(doc, (scope) => {
            capturedScope = scope;
            return {
                get<T>(_key: string, defaultValue: T): T { return defaultValue; },
                has: () => false,
                inspect: () => undefined,
                update: () => Promise.resolve(),
            } as unknown as vscode.WorkspaceConfiguration;
        });

        assert.ok(
            capturedScope !== undefined &&
            typeof capturedScope === 'object' &&
            !(capturedScope instanceof vscode.Uri) &&
            'languageId' in capturedScope,
            `getConfiguration scope must include languageId for language-scoped settings; got: ${JSON.stringify(capturedScope)}`,
        );
        assert.strictEqual(
            (capturedScope as { languageId: string }).languageId,
            'r',
            'languageId in scope must match the document language',
        );
    });

    test('resolveTabSizeForDocument returns tab size from configuration', () => {
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        const tabSize = resolveTabSizeForDocument(doc, () => ({
            get<T>(key: string, defaultValue: T): T {
                if (key === 'tabSize') return 4 as unknown as T;
                return defaultValue;
            },
            has: () => true,
            inspect: () => undefined,
            update: () => Promise.resolve(),
        } as unknown as vscode.WorkspaceConfiguration));
        assert.strictEqual(tabSize, 4);
    });

    test('resolveTabSizeForDocument prefers resolved visible editor tab size', () => {
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        const tabSize = resolveTabSizeForDocument(
            doc,
            () => ({
                get<T>(_key: string, defaultValue: T): T { return defaultValue; },
                has: () => true,
                inspect: () => undefined,
                update: () => Promise.resolve(),
            } as unknown as vscode.WorkspaceConfiguration),
            [
                {
                    document: doc,
                    options: { tabSize: 4 },
                } as unknown as vscode.TextEditor,
            ],
        );
        assert.strictEqual(tabSize, 4);
    });

    test('planDotInWordMigration migrates an explicit old value to the new key', () => {
        const actions = planDotInWordMigration(
            { globalValue: 'no' },
            undefined,
        );
        assert.deepStrictEqual(actions, [
            { target: vscode.ConfigurationTarget.Global, newValue: 'no' },
        ]);
    });

    test('planDotInWordMigration is a no-op when the old key is unset', () => {
        assert.deepStrictEqual(planDotInWordMigration(undefined, undefined), []);
        assert.deepStrictEqual(
            planDotInWordMigration({ globalValue: undefined }, { globalValue: 'yes' }),
            [],
        );
    });

    test('planDotInWordMigration lets the new key win and only clears the old', () => {
        // Both set at the same scope: new value is kept (newValue undefined =>
        // do not overwrite), old key still gets cleared at that scope.
        const actions = planDotInWordMigration(
            { globalValue: 'yes' },
            { globalValue: 'no' },
        );
        assert.deepStrictEqual(actions, [
            { target: vscode.ConfigurationTarget.Global, newValue: undefined },
        ]);
    });

    test('planDotInWordMigration migrates per scope independently', () => {
        // Old set at Global (new unset there) and at WorkspaceFolder (new also
        // set there); Workspace untouched. Global migrates the value; the
        // workspace-folder scope only clears the old key.
        const actions = planDotInWordMigration(
            { globalValue: 'yes', workspaceFolderValue: 'no' },
            { workspaceFolderValue: 'yes' },
        );
        assert.deepStrictEqual(actions, [
            { target: vscode.ConfigurationTarget.Global, newValue: 'yes' },
            { target: vscode.ConfigurationTarget.WorkspaceFolder, newValue: undefined },
        ]);
    });

    test('planDotInWordMigration honors a restricted target list', () => {
        // The workspace-wide pass must ignore workspaceFolderValue (it only
        // resolves on a resource-scoped configuration) even when present.
        const actions = planDotInWordMigration(
            { globalValue: 'no', workspaceFolderValue: 'ask' },
            undefined,
            [vscode.ConfigurationTarget.Global, vscode.ConfigurationTarget.Workspace],
        );
        assert.deepStrictEqual(actions, [
            { target: vscode.ConfigurationTarget.Global, newValue: 'no' },
        ]);
    });

    test('planDotInWordMigration plans only the folder scope when so targeted', () => {
        const actions = planDotInWordMigration(
            { globalValue: 'no', workspaceFolderValue: 'ask' },
            undefined,
            [vscode.ConfigurationTarget.WorkspaceFolder],
        );
        assert.deepStrictEqual(actions, [
            { target: vscode.ConfigurationTarget.WorkspaceFolder, newValue: 'ask' },
        ]);
    });

    test('getUpdatedGlobalLanguageConfig ignores workspace-only overrides', () => {
        assert.deepStrictEqual(
            getUpdatedGlobalLanguageConfig(
                {
                    globalValue: undefined,
                    workspaceValue: {
                        'editor.tabSize': 8,
                        'editor.wordSeparators': 'workspace-only',
                    },
                    workspaceFolderValue: {
                        'editor.insertSpaces': false,
                    },
                },
                'abc',
            ),
            { 'editor.wordSeparators': 'abc' },
        );
    });
});
