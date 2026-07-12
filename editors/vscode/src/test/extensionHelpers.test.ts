import * as assert from 'assert';
import * as vscode from 'vscode';
import {
    buildDocumentIndentUnitsPayload,
    clearIneligibleDiagnostics,
    diagnosticResourceUris,
    getUpdatedGlobalLanguageConfig,
    invalidateResolvedEditorOptions,
    isIndentUnitDocument,
    isRDocument,
    planDotInWordMigration,
    resolveFormatOnTypeForDocument,
    resolveInsertSpacesForDocument,
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

    test('diagnosticResourceUris keeps an independent peek of an active diff original', () => {
        // One active diff renders exactly one visible editor for its original
        // side; a second visible editor with the same URI must come from an
        // independent element (e.g. a peek editor in another group) and must
        // count.
        const original = vscode.Uri.file('/tmp/active-diff-original.R');
        const modified = vscode.Uri.file('/tmp/active-diff-modified.R');

        const result = diagnosticResourceUris(
            [{
                tabs: [
                    { input: { original, modified }, isActive: true },
                ],
            }],
            [
                { document: { uri: original } },
                { document: { uri: modified } },
                { document: { uri: original } },
            ],
        );

        assert.deepStrictEqual(result, [
            modified.toString(),
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

    test('isIndentUnitDocument accepts untitled chunk-capable documents', () => {
        const makeUntitledDocument = (
            languageId: string,
        ): Pick<vscode.TextDocument, 'isUntitled' | 'languageId' | 'uri'> => ({
            isUntitled: true,
            languageId,
            uri: vscode.Uri.parse(`untitled:${languageId}`),
        });

        assert.strictEqual(isIndentUnitDocument(makeUntitledDocument('r')), true);
        assert.strictEqual(isIndentUnitDocument(makeUntitledDocument('jags')), true);
        assert.strictEqual(isIndentUnitDocument(makeUntitledDocument('stan')), true);
        assert.strictEqual(isIndentUnitDocument(makeUntitledDocument('rmd')), true);
        assert.strictEqual(isIndentUnitDocument(makeUntitledDocument('quarto')), true);
        assert.strictEqual(isIndentUnitDocument(makeUntitledDocument('plaintext')), false);
    });

    test('isIndentUnitDocument accepts supported file-backed extensions', () => {
        const makeFileDocument = (
            filePath: string,
            languageId = 'plaintext',
        ): Pick<vscode.TextDocument, 'isUntitled' | 'languageId' | 'uri'> => ({
            isUntitled: false,
            languageId,
            uri: vscode.Uri.file(filePath),
        });

        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/script.R')), true);
        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/model.BUGS')), true);
        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/model.StAn')), true);
        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/report.Rmd')), true);
        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/report.rmarkdown')), true);
        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/report.qmd')), true);
        assert.strictEqual(isIndentUnitDocument(makeFileDocument('/tmp/notes.txt')), false);
        assert.strictEqual(
            isIndentUnitDocument(makeFileDocument('/tmp/no-extension', 'r')),
            true,
        );
        assert.strictEqual(
            isIndentUnitDocument(makeFileDocument('/tmp/notes.txt', 'rmd')),
            true,
        );
        assert.strictEqual(
            isIndentUnitDocument(makeFileDocument('/tmp/notes.txt', 'quarto')),
            true,
        );
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
        }, [], new Map());

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
        } as unknown as vscode.WorkspaceConfiguration), [], new Map());
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
            new Map(),
        );
        assert.strictEqual(tabSize, 4);
    });

    test('resolveInsertSpacesForDocument passes language-scoped configuration scope', () => {
        const doc = {
            uri: vscode.Uri.file('/proj/foo.R'),
            languageId: 'r',
        };

        let capturedScope: vscode.ConfigurationScope | undefined;
        resolveInsertSpacesForDocument(doc, (scope) => {
            capturedScope = scope;
            return {
                get<T>(_key: string, defaultValue: T): T { return defaultValue; },
                has: () => false,
                inspect: () => undefined,
                update: () => Promise.resolve(),
            } as unknown as vscode.WorkspaceConfiguration;
        }, [], new Map());

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

    test('resolveInsertSpacesForDocument returns insertSpaces from configuration', () => {
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        const insertSpaces = resolveInsertSpacesForDocument(doc, () => ({
            get<T>(key: string, defaultValue: T): T {
                if (key === 'insertSpaces') return false as unknown as T;
                return defaultValue;
            },
            has: () => true,
            inspect: () => undefined,
            update: () => Promise.resolve(),
        } as unknown as vscode.WorkspaceConfiguration), [], new Map());
        assert.strictEqual(insertSpaces, false);
    });

    test('resolveInsertSpacesForDocument prefers resolved visible editor value', () => {
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        const insertSpaces = resolveInsertSpacesForDocument(
            doc,
            () => ({
                // Configuration says spaces; the visible editor's resolved
                // options (tabs) must win.
                get<T>(_key: string, defaultValue: T): T { return defaultValue; },
                has: () => true,
                inspect: () => undefined,
                update: () => Promise.resolve(),
            } as unknown as vscode.WorkspaceConfiguration),
            [
                {
                    document: doc,
                    options: { insertSpaces: false },
                } as unknown as vscode.TextEditor,
            ],
            new Map(),
        );
        assert.strictEqual(insertSpaces, false);
    });

    test('resolveInsertSpacesForDocument ignores an unresolved editor value', () => {
        // TextEditorOptions types insertSpaces as boolean | string; a
        // non-boolean (unresolved "auto") must fall through to configuration.
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        const insertSpaces = resolveInsertSpacesForDocument(
            doc,
            () => ({
                get<T>(key: string, defaultValue: T): T {
                    if (key === 'insertSpaces') return false as unknown as T;
                    return defaultValue;
                },
                has: () => true,
                inspect: () => undefined,
                update: () => Promise.resolve(),
            } as unknown as vscode.WorkspaceConfiguration),
            [
                {
                    document: doc,
                    options: { insertSpaces: 'auto' },
                } as unknown as vscode.TextEditor,
            ],
            new Map(),
        );
        assert.strictEqual(insertSpaces, false);
    });

    test('resolvers remember a hidden document\'s last-seen editor options', () => {
        // Per-editor options (detectIndentation, status-bar overrides) are
        // observable only while an editor is visible. Once the tab is
        // hidden, a rebuild must reuse the remembered values instead of
        // regressing to configuration.
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        const cache = new Map();
        const configSaysSpacesUnit2 = () => ({
            get<T>(key: string, defaultValue: T): T {
                if (key === 'insertSpaces') return true as unknown as T;
                if (key === 'tabSize') return 2 as unknown as T;
                return defaultValue;
            },
            has: () => true,
            inspect: () => undefined,
            update: () => Promise.resolve(),
        } as unknown as vscode.WorkspaceConfiguration);
        const visibleTabsEditor = [
            {
                document: doc,
                options: { insertSpaces: false, tabSize: 8 },
            } as unknown as vscode.TextEditor,
        ];

        assert.strictEqual(
            resolveInsertSpacesForDocument(doc, configSaysSpacesUnit2, visibleTabsEditor, cache),
            false,
        );
        assert.strictEqual(
            resolveTabSizeForDocument(doc, configSaysSpacesUnit2, visibleTabsEditor, cache),
            8,
        );

        // Hidden now: the remembered editor values win over configuration.
        assert.strictEqual(
            resolveInsertSpacesForDocument(doc, configSaysSpacesUnit2, [], cache),
            false,
        );
        assert.strictEqual(
            resolveTabSizeForDocument(doc, configSaysSpacesUnit2, [], cache),
            8,
        );

        // A different document shares no memo.
        const other = { uri: vscode.Uri.file('/proj/other.R'), languageId: 'r' };
        assert.strictEqual(
            resolveInsertSpacesForDocument(other, configSaysSpacesUnit2, [], cache),
            true,
        );
    });

    test('editor configuration changes invalidate only the affected cached fields', () => {
        const cache = new Map([
            ['file:///one.R', { tabSize: 2, insertSpaces: true }],
            ['file:///two.R', { tabSize: 4 }],
        ]);

        invalidateResolvedEditorOptions({ tabSize: new Set(['file:///one.R']) }, cache);
        assert.deepStrictEqual(
            [...cache.entries()],
            [
                ['file:///one.R', { insertSpaces: true }],
                ['file:///two.R', { tabSize: 4 }],
            ],
            'resource-scoped invalidation must preserve other documents and fields',
        );

        invalidateResolvedEditorOptions({ insertSpaces: true, tabSize: true }, cache);
        assert.strictEqual(cache.size, 0, 'global invalidation must clear both fields');
    });

    test('resolveFormatOnTypeForDocument uses language- and resource-scoped configuration', () => {
        const doc = { uri: vscode.Uri.file('/proj/foo.R'), languageId: 'r' };
        let observedScope: vscode.ConfigurationScope | undefined;
        const enabled = resolveFormatOnTypeForDocument(doc, (scope) => {
            observedScope = scope;
            return {
                get<T>(key: string, defaultValue: T): T {
                    return key === 'formatOnType' ? false as unknown as T : defaultValue;
                },
            } as vscode.WorkspaceConfiguration;
        });
        assert.strictEqual(enabled, false);
        assert.deepStrictEqual(observedScope, { uri: doc.uri, languageId: 'r' });
    });

    test('buildDocumentIndentUnitsPayload preserves the v0.14 unit contract', () => {
        const rDoc = {
            isUntitled: false,
            languageId: 'r',
            uri: vscode.Uri.file('/proj/foo.R'),
        };
        const otherDoc = {
            isUntitled: false,
            languageId: 'plaintext',
            uri: vscode.Uri.file('/proj/notes.txt'),
        };
        const resolveUnit = () => 4;
        const resolveInsertSpaces = () => false;
        const resolveFormatOnType = () => true;

        const auto = buildDocumentIndentUnitsPayload(
            'auto',
            [rDoc, otherDoc],
            resolveUnit,
            resolveInsertSpaces,
            resolveFormatOnType,
        );
        assert.deepStrictEqual(auto, {
            units: [{
                uri: rDoc.uri.toString(),
                indentUnit: 4,
            }],
            options: [{
                uri: rDoc.uri.toString(),
                insertSpaces: false,
                formatOnType: true,
            }],
        });

        // Fixed mode keeps the exact legacy empty unit array even when current
        // producer options need syncing. A v0.14/custom server ignores the
        // new top-level array, so it cannot override project unit 4 with the
        // lower-precedence client unit 2.
        const fixed = buildDocumentIndentUnitsPayload(
            2,
            [rDoc, otherDoc],
            resolveUnit,
            resolveInsertSpaces,
            resolveFormatOnType,
        );
        assert.deepStrictEqual(fixed, {
            units: [],
            options: [{
                uri: rDoc.uri.toString(),
                insertSpaces: false,
                formatOnType: true,
            }],
        });

        // Both producer gates at their defaults collapse the current options
        // array too. Disabling format-on-type alone retains an options entry.
        assert.deepStrictEqual(
            buildDocumentIndentUnitsPayload(
                2, [rDoc, otherDoc], resolveUnit, () => true, () => true,
            ),
            { units: [], options: [] },
        );
        assert.deepStrictEqual(
            buildDocumentIndentUnitsPayload(
                2, [rDoc, otherDoc], resolveUnit, () => true, () => false,
            ),
            {
                units: [],
                options: [{
                    uri: rDoc.uri.toString(),
                    insertSpaces: true,
                    formatOnType: false,
                }],
            },
        );
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
