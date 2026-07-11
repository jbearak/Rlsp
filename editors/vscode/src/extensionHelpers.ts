import * as path from 'path';
import * as vscode from 'vscode';

// Set of language IDs and file extensions that Raven's language server
// processes. `.Rmd` / `.qmd` are intentionally absent: those files use the
// dedicated `rmd` / `quarto` language IDs and the LSP's document selector
// is `r` only, so sending activity or watching their file events would be
// noise. The chunk feature (which spans `.Rmd` / `.qmd`) has its own
// language-aware wiring in `chunks/`.
const R_DOCUMENT_LANGUAGE_IDS = new Set(['r', 'jags', 'stan']);
const R_DOCUMENT_EXTENSIONS = new Set([
    '.r',
    '.jags',
    '.bugs',
    '.stan',
]);

const INDENT_UNIT_DOCUMENT_LANGUAGE_IDS = new Set([
    'r',
    'jags',
    'stan',
    'rmd',
    'quarto',
]);
const INDENT_UNIT_DOCUMENT_EXTENSIONS = new Set([
    '.r',
    '.jags',
    '.bugs',
    '.stan',
    '.rmd',
    '.rmarkdown',
    '.qmd',
]);

type LanguageConfigurationInspection = {
    globalValue?: unknown;
    workspaceValue?: unknown;
    workspaceFolderValue?: unknown;
};

export function isRDocument(
    document: Pick<vscode.TextDocument, 'isUntitled' | 'languageId' | 'uri'>,
): boolean {
    if (document.isUntitled) {
        return R_DOCUMENT_LANGUAGE_IDS.has(document.languageId);
    }

    return R_DOCUMENT_EXTENSIONS.has(path.extname(document.uri.fsPath).toLowerCase());
}

/**
 * Documents whose effective editor tab size must be synchronized with the
 * server. Rmd/Quarto are included because Enter inside an R chunk resolves
 * the judge's indent unit from this per-document map (see
 * `backend.rs::on_type_formatting`'s `judge_indent_unit`). They remain
 * excluded from `isRDocument`, whose activity/diagnostics role is narrower.
 */
export function isIndentUnitDocument(
    document: Pick<vscode.TextDocument, 'isUntitled' | 'languageId' | 'uri'>,
): boolean {
    if (document.isUntitled) {
        return INDENT_UNIT_DOCUMENT_LANGUAGE_IDS.has(document.languageId);
    }

    return INDENT_UNIT_DOCUMENT_EXTENSIONS.has(
        path.extname(document.uri.fsPath).toLowerCase(),
    );
}

type TabLike = { input: unknown; isActive?: boolean };
type TabGroupLike = { tabs: readonly TabLike[] };
type VisibleEditorLike = { document: { uri: vscode.Uri } };

/**
 * Return the resources that are genuinely represented in the editor UI.
 *
 * `workspace.textDocuments` is deliberately not used: other extensions can
 * create hidden text models with `workspace.openTextDocument()`. Those models
 * must remain synchronized with Raven for cross-file analysis, but should not
 * acquire their own Problems entries. Diff tabs contribute their modified
 * resource, matching vscode-languageclient's pull-diagnostics policy. Other
 * visible editors are unioned in so peek editors count even though they have
 * no tab; a diff's visible original side is excluded from that union. Only
 * the active tab of a group can render its editors, so only active diff tabs
 * contribute to that exclusion — an inactive diff must not suppress the same
 * resource shown independently (e.g. in a peek editor). The exclusion is
 * counted per occurrence, not per URI: one active diff-original accounts for
 * exactly one visible editor, so a second visible editor with the same URI
 * (an independent peek) still counts.
 *
 * The tab input inspection is structural so newer VS Code resource-backed tab
 * kinds automatically work when they expose `uri` or `modified`.
 */
export function diagnosticResourceUris(
    tabGroups: readonly TabGroupLike[] = vscode.window.tabGroups.all,
    visibleEditors: readonly VisibleEditorLike[] = vscode.window.visibleTextEditors,
): string[] {
    const uris = new Set<string>();
    const diffOriginalCounts = new Map<string, number>();

    for (const group of tabGroups) {
        for (const tab of group.tabs) {
            if (typeof tab.input !== 'object' || tab.input === null) {
                continue;
            }
            const input = tab.input as {
                original?: unknown;
                modified?: unknown;
                uri?: unknown;
            };
            const resource = input.modified instanceof vscode.Uri
                ? input.modified
                : input.uri instanceof vscode.Uri
                    ? input.uri
                    : undefined;
            if (resource) {
                uris.add(resource.toString());
            }
            if (
                tab.isActive
                && input.modified instanceof vscode.Uri
                && input.original instanceof vscode.Uri
            ) {
                const key = input.original.toString();
                diffOriginalCounts.set(key, (diffOriginalCounts.get(key) ?? 0) + 1);
            }
        }
    }

    for (const editor of visibleEditors) {
        const uri = editor.document.uri.toString();
        const remaining = diffOriginalCounts.get(uri) ?? 0;
        if (remaining > 0) {
            // This occurrence is attributable to the active diff's own
            // rendered original side; consume it so an additional visible
            // editor with the same URI still counts.
            diffOriginalCounts.set(uri, remaining - 1);
            continue;
        }
        uris.add(uri);
    }

    return [...uris];
}

/**
 * Remove diagnostics retained for resources outside the current editor-owned
 * set. vscode-languageclient deliberately reuses its push-diagnostic
 * collection after an automatic server restart, so server-side clearing alone
 * cannot reconcile tabs that changed while the server was unavailable.
 */
export function clearIneligibleDiagnostics(
    collection: vscode.DiagnosticCollection | undefined,
    eligibleUris: readonly string[],
): void {
    if (!collection) {
        return;
    }

    const eligible = new Set(eligibleUris);
    const stale: vscode.Uri[] = [];
    collection.forEach((uri) => {
        if (!eligible.has(uri.toString())) {
            stale.push(uri);
        }
    });
    for (const uri of stale) {
        collection.delete(uri);
    }
}

/**
 * Resolve the effective `editor.tabSize` for a document. The scope MUST
 * include `languageId` so VS Code returns language-scoped overrides (e.g.
 * `[r] { "editor.tabSize": 2 }`) instead of only the resource-scoped value.
 *
 * The optional `getCfg` parameter exists for unit testing; callers should
 * omit it and let the default use `vscode.workspace.getConfiguration`.
 */
export function resolveTabSizeForDocument(
    document: Pick<vscode.TextDocument, 'uri' | 'languageId'>,
    getCfg: (scope: vscode.ConfigurationScope) => vscode.WorkspaceConfiguration = (scope) =>
        vscode.workspace.getConfiguration('editor', scope),
    visibleTextEditors: readonly vscode.TextEditor[] = vscode.window.visibleTextEditors,
): number {
    const editor = visibleTextEditors.find((candidate) =>
        candidate.document.uri.toString() === document.uri.toString()
    );
    if (typeof editor?.options.tabSize === 'number') {
        return editor.options.tabSize;
    }

    return getCfg({ uri: document.uri, languageId: document.languageId })
        .get<number>('tabSize', 2);
}

export type DotInWordMigrationAction = {
    target: vscode.ConfigurationTarget;
    /**
     * Value to write to `editor.dotInWord` at this scope, or `undefined` to
     * leave the new key untouched and only clear the deprecated old key.
     */
    newValue?: string;
};

/**
 * Plan the migration from the deprecated `raven.editor.dotInWordSeparators` to
 * `raven.editor.dotInWord`, scope by scope.
 *
 * For each scope (Global / Workspace / WorkspaceFolder) where the old key is
 * explicitly set, the old key must be cleared; if the new key is not already
 * set at that scope, the old value is copied to it (the new key wins when both
 * are set). The returned actions are idempotent — once the old key is gone the
 * plan is empty — so the caller can run this on every activation, which also
 * catches a stale old key re-introduced by Settings Sync.
 *
 * `targets` restricts which scopes are considered, and which `inspect` field
 * each maps to. `workspaceFolderValue` is only meaningful on a resource-scoped
 * configuration, so the caller must pass the `WorkspaceFolder` target together
 * with a folder-scoped `inspect` result (and omit it from the unscoped pass) —
 * see `migrateDotInWordSetting` in `extension.ts`.
 *
 * Pure so it can be unit-tested without a live VS Code configuration.
 */
export function planDotInWordMigration(
    oldInspect: LanguageConfigurationInspection | undefined,
    newInspect: LanguageConfigurationInspection | undefined,
    targets: vscode.ConfigurationTarget[] = [
        vscode.ConfigurationTarget.Global,
        vscode.ConfigurationTarget.Workspace,
        vscode.ConfigurationTarget.WorkspaceFolder,
    ],
): DotInWordMigrationAction[] {
    const keyByTarget = new Map<vscode.ConfigurationTarget, keyof LanguageConfigurationInspection>([
        [vscode.ConfigurationTarget.Global, 'globalValue'],
        [vscode.ConfigurationTarget.Workspace, 'workspaceValue'],
        [vscode.ConfigurationTarget.WorkspaceFolder, 'workspaceFolderValue'],
    ]);

    const actions: DotInWordMigrationAction[] = [];
    for (const target of targets) {
        const key = keyByTarget.get(target);
        if (key === undefined) {
            continue;
        }
        const oldValue = oldInspect?.[key];
        if (oldValue === undefined) {
            continue;
        }
        const newValue = newInspect?.[key];
        actions.push({
            target,
            newValue: newValue === undefined ? (oldValue as string) : undefined,
        });
    }
    return actions;
}

export function getUpdatedGlobalLanguageConfig(
    inspection: LanguageConfigurationInspection | undefined,
    wordSeparators: string,
): Record<string, unknown> | null {
    const globalValue: Record<string, unknown> =
        typeof inspection?.globalValue === 'object' && inspection.globalValue !== null
            ? inspection.globalValue as Record<string, unknown>
            : {};

    if (globalValue['editor.wordSeparators'] === wordSeparators) {
        return null;
    }

    return {
        ...globalValue,
        'editor.wordSeparators': wordSeparators,
    };
}
