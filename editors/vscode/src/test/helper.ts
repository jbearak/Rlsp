import * as vscode from 'vscode';
import * as path from 'path';

export function getFixturePath(name: string): string {
    // __dirname is out/test, fixtures are in src/test/fixtures
    return path.join(__dirname, '..', '..', 'src', 'test', 'fixtures', name);
}

export function getFixtureUri(name: string): vscode.Uri {
    return vscode.Uri.file(getFixturePath(name));
}

export async function activate(): Promise<void> {
    const ext = vscode.extensions.getExtension('jbearak.raven-r');
    if (!ext) {
        // Surface this loudly; otherwise tests that depend on extension
        // activation (registerKnit -> initSessionState) silently skip
        // setup and fail later in confusing places.
        throw new Error(
            `Test helper: vscode.extensions.getExtension('jbearak.raven-r') returned undefined. ` +
                `The extension under test isn't loaded — check .vscode-test.mjs.`,
        );
    }
    if (!ext.isActive) {
        await ext.activate();
    }
    // Belt-and-braces: knit tests routinely fail with "Raven knit session
    // state not initialized" because the extension's `activate` runs out
    // of the bundled `dist/extension.js`, whereas Mocha tests `require`
    // the same module via `out/knit/session-state.js`. The two paths
    // resolve to distinct JS modules with separate `state` singletons,
    // so the `initSessionState` call inside `registerKnit` doesn't
    // populate the singleton the tests read from. Initialize the
    // test-side instance explicitly using the same workspaceUri logic
    // as `knit/index.ts` so paths line up.
    //
    // Cleanup: a fixed sessionId ('raven-test-suite') means a single
    // temp dir is reused across the entire test run, so we never grow
    // an orphan dir per Mocha invocation. The production-side
    // `cleanupCurrentSession` (registered on the extension context's
    // disposal) handles its OWN, separate session id at shutdown. The
    // `sweepStaleSessions` >7-day sweep at the next activation handles
    // anything either side leaves behind.
    const { maybeCurrentSession, initSessionState } = await import('../knit/session-state');
    if (maybeCurrentSession() === null) {
        const workspaceUri =
            vscode.workspace.workspaceFolders?.[0]?.uri.toString()
            ?? vscode.workspace.workspaceFile?.toString()
            ?? null;
        initSessionState({ sessionId: 'raven-test-suite', workspaceUri });
    }
}

export async function openDocument(fixtureName: string): Promise<vscode.TextDocument> {
    const uri = getFixtureUri(fixtureName);
    const doc = await vscode.workspace.openTextDocument(uri);
    await vscode.window.showTextDocument(doc);
    // Give LSP time to process
    await sleep(2000);
    return doc;
}

export async function waitForDiagnostics(uri: vscode.Uri, timeout = 10000): Promise<vscode.Diagnostic[]> {
    const start = Date.now();
    while (Date.now() - start < timeout) {
        const diagnostics = vscode.languages.getDiagnostics(uri);
        if (diagnostics.length > 0) {
            return diagnostics;
        }
        await sleep(200);
    }
    return vscode.languages.getDiagnostics(uri);
}

/**
 * Poll a URI's diagnostics until `predicate` holds or `timeout` elapses,
 * returning the most-recently observed set either way so the caller can assert
 * against it (and produce a useful failure message) on timeout.
 *
 * Prefer this over `waitForDiagnostics` whenever the expected set depends on a
 * setting the test just changed. `waitForDiagnostics` returns on the first
 * *non-empty* batch, but `config.update()` resolves before the extension has
 * pushed the new settings to the server and the server has re-published — so
 * the first batch observed can still reflect the pre-change configuration. For
 * an opt-in diagnostic stream the pre-change state is "no findings", which is
 * indistinguishable from "not computed yet" unless you poll for the expected
 * shape.
 */
export async function waitForDiagnosticsMatching(
    uri: vscode.Uri,
    predicate: (diagnostics: vscode.Diagnostic[]) => boolean,
    timeout = 10000,
): Promise<vscode.Diagnostic[]> {
    const start = Date.now();
    let diagnostics = vscode.languages.getDiagnostics(uri);
    while (Date.now() - start < timeout) {
        diagnostics = vscode.languages.getDiagnostics(uri);
        if (predicate(diagnostics)) {
            return diagnostics;
        }
        await sleep(200);
    }
    return diagnostics;
}

export function sleep(ms: number): Promise<void> {
    return new Promise(resolve => setTimeout(resolve, ms));
}

/**
 * Ensure `vscode.window.activeTextEditor` is an editor for `editor.document`.
 *
 * `vscode.window.showTextDocument(doc)`'s promise resolves before VS Code
 * has finished promoting the new editor to "active" — under suite-cumulative
 * load (other webviews focused, prior tests' panels lingering, output
 * channels stealing focus), the `activeTextEditor` pointer can still
 * reference a different editor (or `undefined`) when the next
 * `executeCommand` runs. Command handlers that read
 * `vscode.window.activeTextEditor` then operate on the wrong document and
 * produce confusing failures like "no R chunks found in this document".
 *
 * Strategy: poll up to ~250 ms (handles the common "just resolved a tick
 * ago" race cheaply), and on any longer delay actively re-focus by
 * re-calling `showTextDocument`. Output channels (`output:` scheme) and
 * other non-test editors do not yield to a polling-only wait — they have
 * to be displaced explicitly: when the active editor is an output channel
 * (commonly `output:tasks` from VS Code's npm-task auto-detection),
 * `showTextDocument` alone leaves focus on the panel even after the new
 * editor opens, so we close the panel and refocus the editor group first.
 *
 * Comparison is by document URI, not by `TextEditor` reference identity.
 * `showTextDocument` on an already-open document is documented to return
 * "an existing editor that is showing the document", but VS Code does not
 * guarantee the returned object is `===` to the caller's prior reference —
 * a strict-equality wait would spin to the timeout in those cases even
 * though the right document is focused.
 *
 * Call this right after `showTextDocument`, and before any `executeCommand`
 * that depends on the active editor.
 */
export async function awaitActive(
    editor: vscode.TextEditor,
    timeoutMs = 5000,
): Promise<void> {
    const targetUri = editor.document.uri.toString();
    const matches = (): boolean => {
        const active = vscode.window.activeTextEditor;
        return active !== undefined && active.document.uri.toString() === targetUri;
    };
    const started = Date.now();
    let lastForceFocusAt = 0;
    while (!matches()) {
        const elapsed = Date.now() - started;
        if (elapsed > timeoutMs) {
            const active = vscode.window.activeTextEditor?.document.uri.toString() ?? 'none';
            throw new Error(
                `Editor for ${targetUri} never became active (active: ${active})`,
            );
        }
        // Re-focus the test's document after a short initial poll budget,
        // then at most every 500 ms. The two-phase shape keeps the common
        // case (`showTextDocument` just resolved) cheap, while displacing
        // a focused output channel or webview when polling alone won't.
        if (elapsed >= 250 && Date.now() - lastForceFocusAt >= 500) {
            lastForceFocusAt = Date.now();
            // If the current active is an output channel (e.g.
            // `output:tasks` opened by VS Code's npm-task auto-detection
            // on startup), `showTextDocument` opens the editor but does
            // not steal focus back from the panel. Close the panel and
            // pin focus to the editor group before re-showing so the
            // newly opened editor actually wins activation.
            const activeScheme = vscode.window.activeTextEditor?.document.uri.scheme;
            if (activeScheme === 'output') {
                try {
                    await vscode.commands.executeCommand('workbench.action.closePanel');
                } catch { /* panel may already be closed */ }
                try {
                    await vscode.commands.executeCommand('workbench.action.focusActiveEditorGroup');
                } catch { /* no editor group yet — showTextDocument below handles it */ }
            }
            try {
                await vscode.window.showTextDocument(editor.document, {
                    viewColumn: editor.viewColumn,
                    preserveFocus: false,
                    preview: false,
                });
            } catch {
                // Document may have been closed externally; the next poll
                // tick will detect the mismatch and time out with a
                // diagnostic error rather than swallowing this silently.
            }
        }
        await sleep(25);
    }
}

/**
 * Delete the fixture workspace's `.vscode/settings.json` when it holds no
 * keys, plus the `.vscode` directory when that leaves it empty.
 *
 * `config.update(key, undefined, Workspace)` clears the key but VS Code
 * leaves the file on disk, possibly as a bare `{}`. Any test that writes a
 * workspace-scoped setting must sweep it, or the fixture tree ends up dirty
 * in `git status`. A file with real content is left untouched.
 */
export async function removeEmptyWorkspaceSettings(): Promise<void> {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) return;
    // VS Code's write after `update(key, undefined)` is best-effort and
    // asynchronous; give the pending write a moment to land before we look.
    await sleep(200);
    const dotVscode = vscode.Uri.joinPath(folder.uri, '.vscode');
    const settings = vscode.Uri.joinPath(dotVscode, 'settings.json');
    const remove = async (uri: vscode.Uri): Promise<void> => {
        try {
            await vscode.workspace.fs.delete(uri);
        } catch {
            // best-effort cleanup
        }
    };
    try {
        const bytes = await vscode.workspace.fs.readFile(settings);
        const text = Buffer.from(bytes).toString('utf8').trim();
        if (text !== '' && text !== '{}' && text !== '{\n}') return;
        await remove(settings);
        try {
            const entries = await vscode.workspace.fs.readDirectory(dotVscode);
            if (entries.length === 0) await remove(dotVscode);
        } catch {
            // .vscode missing — nothing else to clean.
        }
    } catch {
        // No settings.json — nothing to clean.
    }
}
