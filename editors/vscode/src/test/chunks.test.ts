import * as assert from 'assert';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { randomUUID } from 'node:crypto';
import * as vscode from 'vscode';
import { awaitActive } from './helper';

const CHUNK_COMMANDS = [
    'raven.runCurrentChunk',
    'raven.runCurrentChunkAndMove',
    'raven.runAboveChunks',
    'raven.runAllChunks',
    'raven.runCurrentAndBelowChunks',
    'raven.runBelowChunks',
    'raven.runPreviousChunk',
    'raven.runPreviousChunkAndMove',
    'raven.runNextChunk',
    'raven.runNextChunkAndMove',
    'raven.goToNextChunk',
    'raven.goToPreviousChunk',
    'raven.selectCurrentChunk',
] as const;

const RMD_FIXTURE = [
    '---',
    'title: example',
    '---',
    '',
    'Some prose.',
    '',
    '```{r setup, include=FALSE}',
    'library(dplyr)',
    '```',
    '',
    'More prose.',
    '',
    '```{python}',
    'print("not r")',
    '```',
    '',
    '```{r second}',
    'x <- 1',
    'y <- 2',
    '```',
    '',
    '```{r noeval, eval=FALSE}',
    'never_run()',
    '```',
    '',
].join('\n');

const R_CELL_FIXTURE = [
    '# %% one',
    'a <- 1',
    'b <- 2',
    '# %% two',
    'c <- 3',
    '',
].join('\n');

const MARKDOWN_FIXTURE = [
    '# Technical notes',
    '',
    '```r',
    'summary(cars)',
    '```',
    '',
    '```python',
    'print("not R")',
    '```',
    '',
].join('\n');

const TEMP_FIXTURE_FILES: string[] = [];

/**
 * Open a document with the supplied content for the chunk detector:
 *   - `'rmd'` (default) — R Markdown / Quarto fenced-block mode. Written
 *     to a temp `.rmd` file so the URI-extension path of the classifier
 *     fires reliably. The extension now contributes the `'rmd'` language
 *     ID natively (so the languageId path would work too), but a real
 *     `.rmd` file exercises both paths and sidesteps a Linux VS Code
 *     1.120 quirk where untitled buffers can drop a freshly-contributed
 *     languageId when the extension host hasn't loaded yet.
 *   - `'markdown'` — regular Markdown fenced-code mode. Written to `.md`.
 *   - `'r'` — plain R / `# %%` cell-marker mode. Uses an untitled buffer
 *     because `'r'` has been a registered languageId since day one and
 *     reliably survives the round-trip.
 */
async function open_doc(
    content: string,
    language: 'rmd' | 'markdown' | 'r' = 'rmd',
): Promise<vscode.TextEditor> {
    let editor: vscode.TextEditor;
    if (language === 'rmd' || language === 'markdown') {
        const extension = language === 'rmd' ? 'rmd' : 'md';
        const tmp = path.join(os.tmpdir(), `raven-chunks-${randomUUID()}.${extension}`);
        fs.writeFileSync(tmp, content);
        TEMP_FIXTURE_FILES.push(tmp);
        const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(tmp));
        editor = await vscode.window.showTextDocument(doc);
    } else {
        const doc = await vscode.workspace.openTextDocument({ language, content });
        editor = await vscode.window.showTextDocument(doc);
    }
    // Without this wait, `vscode.window.activeTextEditor` can still point at
    // a prior test's editor (or a webview-focused `undefined`) when the next
    // `executeCommand` runs — chunk handlers then read the wrong document and
    // produce "no R chunks found in this document". See `awaitActive` doc.
    await awaitActive(editor);
    return editor;
}

function place_cursor(editor: vscode.TextEditor, line: number, char = 0): void {
    const pos = new vscode.Position(line, char);
    editor.selection = new vscode.Selection(pos, pos);
}

/**
 * Re-pin `editor` as the active editor, then invoke a Raven command.
 *
 * Every chunk command we exercise here reads
 * `vscode.window.activeTextEditor` to find the document to operate on
 * (`run_chunk` in `chunk-commands.ts`, the chunk-navigation handlers).
 * Any await between `open_doc` and the command call (e.g.
 * `dispose_any_cached_r_terminal`'s 200 ms sleep) is an opportunity for
 * VS Code to steal focus, leaving `activeTextEditor` `undefined` or
 * pointing at a different editor. When that happens the command runs
 * against the wrong document and silently produces "no R chunks found
 * in this document". Re-pinning right before the call closes that race.
 */
async function execute_chunk_command(
    editor: vscode.TextEditor,
    command: string,
): Promise<void> {
    await awaitActive(editor);
    await vscode.commands.executeCommand(command);
}

interface MessageStub {
    last: string | undefined;
    restore(): void;
}

function stub_information_message(): MessageStub {
    const original = vscode.window.showInformationMessage;
    const result: MessageStub = {
        last: undefined,
        restore: () => {
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            (vscode.window as any).showInformationMessage = original;
        },
    };
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (vscode.window as any).showInformationMessage = (msg: string) => {
        result.last = msg;
        return Promise.resolve(undefined);
    };
    return result;
}

interface TerminalStub {
    sent: string[];
    restore(): void;
}

/**
 * Names the bundled R terminal uses today (`r-terminal-manager.ts`
 * `TERMINAL_NAME`). We dispose any pre-existing terminal with this name
 * before installing the recording stub so the bundled extension's
 * module-level cache doesn't hold a real terminal created by an earlier
 * test suite (e.g. the data-viewer smoke tests).
 */
const RAVEN_R_TERMINAL_NAME = 'R (Raven)';

async function dispose_any_cached_r_terminal(): Promise<void> {
    const stale = vscode.window.terminals.filter(
        (t) => t.name === RAVEN_R_TERMINAL_NAME,
    );
    if (stale.length === 0) return;
    for (const t of stale) {
        t.dispose();
    }
    // `dispose()` resolves before `onDidCloseTerminal` fires inside the
    // bundled extension and clears its `last_active_terminal` cache, so
    // wait a tick for that handler to run.
    await new Promise<void>((resolve) => setTimeout(resolve, 200));
}

/**
 * Replace `vscode.window.createTerminal` so the bundled extension's
 * `get_or_create_r_terminal` returns a recording terminal. The extension
 * caches the terminal in-process via module-level state, so the FIRST call
 * after stubbing is what stays — subsequent stub installs in the same suite
 * reuse that same cached terminal.
 *
 * To make the recorded sends reliable across tests, the fake terminal's
 * `sendText` delegates to a module-level recorder. Each `stub_create_terminal`
 * call:
 *   1. resets the recorder to a fresh empty array
 *   2. returns a `TerminalStub` whose `sent` reflects the live state of
 *      `RECORDED_SENT` (so writes via the cached fake terminal land in this
 *      stub's view).
 *
 * The cached fake terminal stays installed across tests — only the recorder
 * target rotates. `restore` is still provided for the `vscode.window.createTerminal`
 * override; the fake terminal itself is intentionally sticky.
 */
let RECORDED_SENT: string[] = [];
const CACHED_FAKE_TERMINAL = {
    name: 'R (Raven test stub)',
    sendText: (text: string, _addNewLine?: boolean) => {
        RECORDED_SENT.push(text);
    },
    show: () => undefined,
    dispose: () => undefined,
    processId: Promise.resolve(undefined),
    creationOptions: { name: 'R (Raven test stub)' },
    exitStatus: undefined,
    state: { isInteractedWith: false },
    // Cast through `unknown` because the VS Code TerminalState/shell-integration
    // interfaces churn across `@types/vscode` versions and the test only needs
    // `sendText` / `show` to be callable.
} as unknown as vscode.Terminal;

function stub_create_terminal(): TerminalStub {
    RECORDED_SENT = [];
    const original = vscode.window.createTerminal;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (vscode.window as any).createTerminal = (..._args: unknown[]) => CACHED_FAKE_TERMINAL;
    return {
        get sent() {
            return RECORDED_SENT;
        },
        restore: () => {
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            (vscode.window as any).createTerminal = original;
        },
    } as TerminalStub;
}

/**
 * Poll `vscode.executeCodeLensProvider` until `predicate(lenses)` is true
 * or `timeoutMs` elapses. Returns the most-recently observed lens array
 * either way so the caller can assert against it on timeout.
 *
 * Use this after mutating settings that the CodeLens provider listens to
 * (e.g. `raven.chunks.codeLens.commands`): `config.update()` resolves
 * before the provider's `onDidChangeConfiguration` handler runs and VS
 * Code's CodeLens cache refreshes, so a fixed sleep is racy.
 */
async function poll_for_lenses(
    uri: vscode.Uri,
    predicate: (lenses: vscode.CodeLens[]) => boolean,
    { timeoutMs = 3000, intervalMs = 50 }: { timeoutMs?: number; intervalMs?: number } = {},
): Promise<vscode.CodeLens[]> {
    const deadline = Date.now() + timeoutMs;
    let lenses: vscode.CodeLens[] = [];
    while (true) {
        lenses = (await vscode.commands.executeCommand<vscode.CodeLens[]>(
            'vscode.executeCodeLensProvider',
            uri,
        )) ?? [];
        if (predicate(lenses)) return lenses;
        if (Date.now() >= deadline) return lenses;
        await new Promise<void>((resolve) => setTimeout(resolve, intervalMs));
    }
}

suite('chunk commands: registration and behavior', () => {
    suiteTeardown(() => {
        for (const file of TEMP_FIXTURE_FILES) {
            try { fs.unlinkSync(file); } catch { /* best-effort */ }
        }
        TEMP_FIXTURE_FILES.length = 0;
    });

    test('every chunk command is registered in VS Code', async () => {
        // Skip when R-console activation is disabled (REditorSupport / Positron):
        // chunk navigation, decorations, and run / CodeLens-positional commands
        // are all gated together so coexistence users don't get duplicate
        // surfaces. The assertion still validates the package.json declarations
        // in the next test, which run unconditionally.
        const all = new Set(await vscode.commands.getCommands(true));
        const r_console_disabled = !all.has('raven.runLineOrSelection');
        if (r_console_disabled) {
            for (const cmd of CHUNK_COMMANDS) {
                assert.ok(
                    !all.has(cmd),
                    `expected chunk command "${cmd}" to be absent when R-console is disabled`,
                );
            }
            return;
        }
        for (const cmd of CHUNK_COMMANDS) {
            assert.ok(
                all.has(cmd),
                `expected chunk command "${cmd}" to be registered`,
            );
        }
    });

    test('package.json declares every chunk command under the Raven category', () => {
        // eslint-disable-next-line @typescript-eslint/no-require-imports
        const pkg = require('../../package.json') as {
            contributes: {
                commands: Array<{ command: string; title: string; category?: string }>;
            };
        };
        const declared = new Map(
            pkg.contributes.commands.map((c) => [c.command, c]),
        );
        for (const cmd of CHUNK_COMMANDS) {
            const entry = declared.get(cmd);
            assert.ok(entry, `package.json must declare ${cmd}`);
            assert.strictEqual(
                entry.category,
                'Raven',
                `${cmd} must be under the Raven category`,
            );
        }
    });

    test('chunk navigation surfaces are gated behind raven.rConsoleEnabled', () => {
        // The navigation commands, their keybindings, and their command-palette
        // entries must all require `raven.rConsoleEnabled` — REditorSupport /
        // Positron ship equivalent chunk navigation, so coexistence users
        // should not see duplicates. See `docs/coexistence.md`.
        // eslint-disable-next-line @typescript-eslint/no-require-imports
        const pkg = require('../../package.json') as {
            contributes: {
                keybindings: Array<{ command: string; when?: string }>;
                menus: { commandPalette: Array<{ command: string; when?: string }> };
            };
        };
        const NAV = ['raven.goToNextChunk', 'raven.goToPreviousChunk', 'raven.selectCurrentChunk'];

        for (const cmd of NAV) {
            const palette = pkg.contributes.menus.commandPalette.find(
                (entry) => entry.command === cmd,
            );
            assert.ok(palette, `command palette must gate ${cmd}`);
            assert.ok(
                palette.when?.includes('raven.rConsoleEnabled'),
                `${cmd} command palette entry must require raven.rConsoleEnabled`,
            );
        }

        for (const cmd of ['raven.goToNextChunk', 'raven.goToPreviousChunk']) {
            const binding = pkg.contributes.keybindings.find(
                (entry) => entry.command === cmd,
            );
            assert.ok(binding, `${cmd} should have a keybinding`);
            assert.ok(
                binding.when?.includes('raven.rConsoleEnabled'),
                `${cmd} keybinding must require raven.rConsoleEnabled`,
            );
        }
    });

    test('goToNextChunk places cursor inside the next R chunk body', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.goToNextChunk');
        if (r_console_disabled) return; // command not registered in this env
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 0); // before any chunk
        await execute_chunk_command(editor, 'raven.goToNextChunk');
        // First chunk header is line 6 ("```{r setup, ...}"), body starts at line 7.
        assert.strictEqual(editor.selection.active.line, 7);

        await execute_chunk_command(editor, 'raven.goToNextChunk');
        // Second runnable chunk header is the python one at 12, but navigation walks
        // every chunk regardless of language — the next chunk is python at 12, so
        // cursor lands on line 13 (body of the python chunk). That's intentional:
        // navigation is language-agnostic.
        assert.strictEqual(editor.selection.active.line, 13);
    });

    test('goToPreviousChunk walks chunks in reverse', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.goToPreviousChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the second R chunk body
        await execute_chunk_command(editor, 'raven.goToPreviousChunk');
        // Previous chunk is python at line 12 → body line 13.
        assert.strictEqual(editor.selection.active.line, 13);
    });

    test('selectCurrentChunk selects only the body of the chunk under the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.selectCurrentChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the "second" R chunk
        await execute_chunk_command(editor, 'raven.selectCurrentChunk');
        const text = editor.document.getText(editor.selection);
        assert.strictEqual(text, 'x <- 1\ny <- 2');
    });

    test('runCurrentChunk warns when cursor is not inside a chunk', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunk');
        if (r_console_disabled) return; // command not registered in this env
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 4); // prose line, not in a chunk
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runCurrentChunk');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('not inside an R chunk'),
            `expected "not inside an R chunk" info message, got: ${String(stub.last)}`,
        );
    });

    test('runCurrentChunk refuses non-R chunks', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 13); // inside the python chunk
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runCurrentChunk');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('python'),
            `expected python-language info message, got: ${String(stub.last)}`,
        );
    });

    test('runCurrentChunk on an empty (no chunks) document warns', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunk');
        if (r_console_disabled) return;
        const editor = await open_doc('x <- 1\nprint(x)\n', 'r');
        place_cursor(editor, 0);
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runCurrentChunk');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('no R chunks'),
            `expected "no R chunks" info message, got: ${String(stub.last)}`,
        );
    });

    test('runCurrentChunk sends the chunk body to the R terminal', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the "second" R chunk
        // Dispose any R terminal a preceding test (e.g. data-viewer smoke
        // tests, which surface `View()` panels and may create an R terminal
        // in the process) left behind. Otherwise the bundled extension
        // returns its cached real terminal and the stub never captures.
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runCurrentChunk');
        } finally {
            term.restore();
        }
        const combined = term.sent.join('\n');
        // The send path may wrap multi-line payloads in bracketed paste markers or
        // a tempfile source() wrapper. Either way, the original chunk body should
        // appear somewhere in the captured text (with chunk-body lines preserved
        // or referenced via a temp-file source path). To keep the assertion
        // robust across send transports, just check that AT LEAST one of the
        // chunk's distinctive identifiers made it to the terminal.
        const observed_chunk_content =
            combined.includes('x <- 1') ||
            combined.includes('y <- 2') ||
            /source\((.+)\.R/.test(combined);
        assert.ok(
            observed_chunk_content,
            `expected chunk body to reach terminal, got: ${combined.slice(0, 200)}`,
        );
    });

    test('# %% cell mode in .R: selectCurrentChunk grabs the cell body', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.selectCurrentChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(R_CELL_FIXTURE, 'r');
        place_cursor(editor, 1); // inside cell "one"
        await execute_chunk_command(editor, 'raven.selectCurrentChunk');
        const text = editor.document.getText(editor.selection);
        assert.strictEqual(text, 'a <- 1\nb <- 2');
    });

    /**
     * Behavioral coverage for the four new run commands (issue #229).
     * Each test stubs the R terminal, places the cursor at a known position
     * in `RMD_FIXTURE`, executes the command, and verifies that the chunk
     * body that *should* have been sent reaches the terminal — matching
     * either the literal text (single-line wrapper) or a `source(.../*.R)`
     * tempfile wrapper for multi-line payloads, the same pattern used by
     * the existing `runCurrentChunk` test.
     */
    function captured_payload(sent: string[]): string {
        return sent.join('\n');
    }

    function payload_contains(sent: string[], needle: string): boolean {
        const text = captured_payload(sent);
        return text.includes(needle) || /source\((.+)\.R/.test(text);
    }

    test('runNextChunk runs the chunk immediately below the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runNextChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 7); // inside the first R chunk ("setup")
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runNextChunk');
        } finally {
            term.restore();
        }
        // Next runnable chunk after "setup" is "second" (python is skipped).
        // Its body is `x <- 1\ny <- 2`.
        assert.ok(
            payload_contains(term.sent, 'x <- 1') ||
            payload_contains(term.sent, 'y <- 2'),
            `expected "second" chunk body to reach terminal, got: ${captured_payload(term.sent).slice(0, 200)}`,
        );
    });

    test('runPreviousChunk runs the chunk immediately above the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runPreviousChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the "second" R chunk
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runPreviousChunk');
        } finally {
            term.restore();
        }
        // Previous runnable chunk before "second" is "setup" (python is skipped).
        // Its body is `library(dplyr)`.
        assert.ok(
            payload_contains(term.sent, 'library(dplyr)'),
            `expected "setup" chunk body to reach terminal, got: ${captured_payload(term.sent).slice(0, 200)}`,
        );
    });

    test('runBelowChunks runs every R chunk after the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runBelowChunks');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 4); // prose line before any chunk
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runBelowChunks');
        } finally {
            term.restore();
        }
        // All three R chunks should be combined into the payload.
        const text = captured_payload(term.sent);
        const has_all_chunks =
            text.includes('library(dplyr)') &&
            text.includes('x <- 1') &&
            text.includes('never_run()');
        const has_tempfile = /source\((.+)\.R/.test(text);
        assert.ok(
            has_all_chunks || has_tempfile,
            `expected all R chunk bodies to reach terminal, got: ${text.slice(0, 300)}`,
        );
    });

    test('runCurrentAndBelowChunks runs the cursor chunk plus every R chunk after it', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentAndBelowChunks');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the "second" R chunk
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runCurrentAndBelowChunks');
        } finally {
            term.restore();
        }
        const text = captured_payload(term.sent);
        const has_current_and_below =
            text.includes('x <- 1') &&
            text.includes('never_run()');
        const has_tempfile = /source\((.+)\.R/.test(text);
        assert.ok(
            has_current_and_below || has_tempfile,
            `expected current ("second") and below ("noeval") chunk bodies to reach terminal, got: ${text.slice(0, 300)}`,
        );
    });

    test('runNextChunk warns when there is no chunk below the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runNextChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 22); // after the last chunk
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runNextChunk');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('no runnable chunk below'),
            `expected "no runnable chunk below" info message, got: ${String(stub.last)}`,
        );
    });

    test('runPreviousChunk warns when there is no chunk above the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runPreviousChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 0); // before any chunk
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runPreviousChunk');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('no runnable chunk above'),
            `expected "no runnable chunk above" info message, got: ${String(stub.last)}`,
        );
    });

    // ── Issue #280: `…AndMove` variants run the next/previous chunk AND jump
    // the cursor + viewport into the chunk that was just run.
    test('runNextChunkAndMove runs the next chunk and moves the cursor into it', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runNextChunkAndMove');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 7); // inside the first R chunk ("setup")
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runNextChunkAndMove');
        } finally {
            term.restore();
        }
        // Next runnable chunk after "setup" is "second" (python is skipped):
        // header at line 16, first body line at 17.
        assert.ok(
            payload_contains(term.sent, 'x <- 1') ||
            payload_contains(term.sent, 'y <- 2'),
            `expected "second" chunk body to reach terminal, got: ${captured_payload(term.sent).slice(0, 200)}`,
        );
        assert.strictEqual(
            editor.selection.active.line,
            17,
            'cursor should land on the first body line of the just-run "second" chunk',
        );
    });

    test('runPreviousChunkAndMove runs the previous chunk and moves the cursor into it', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runPreviousChunkAndMove');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the "second" R chunk
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runPreviousChunkAndMove');
        } finally {
            term.restore();
        }
        // Previous runnable chunk before "second" is "setup" (python is skipped):
        // header at line 6, first body line at 7.
        assert.ok(
            payload_contains(term.sent, 'library(dplyr)'),
            `expected "setup" chunk body to reach terminal, got: ${captured_payload(term.sent).slice(0, 200)}`,
        );
        assert.strictEqual(
            editor.selection.active.line,
            7,
            'cursor should land on the first body line of the just-run "setup" chunk',
        );
    });

    test('runNextChunkAndMove at end of document warns and does not move the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runNextChunkAndMove');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 22); // inside the last (noeval) chunk; no chunk below
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runNextChunkAndMove');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('no runnable chunk below'),
            `expected "no runnable chunk below" info message, got: ${String(stub.last)}`,
        );
        assert.strictEqual(
            editor.selection.active.line,
            22,
            'cursor should not move when there is no next runnable chunk',
        );
    });

    test('runPreviousChunkAndMove at top of document warns and does not move the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runPreviousChunkAndMove');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 0); // before any chunk
        const stub = stub_information_message();
        try {
            await execute_chunk_command(editor, 'raven.runPreviousChunkAndMove');
        } finally {
            stub.restore();
        }
        assert.ok(
            stub.last && stub.last.includes('no runnable chunk above'),
            `expected "no runnable chunk above" info message, got: ${String(stub.last)}`,
        );
        assert.strictEqual(
            editor.selection.active.line,
            0,
            'cursor should not move when there is no previous runnable chunk',
        );
    });

    test('runNextChunk (legacy) does not move the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runNextChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 7); // inside the first R chunk
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runNextChunk');
        } finally {
            term.restore();
        }
        assert.strictEqual(
            editor.selection.active.line,
            7,
            'legacy runNextChunk must not move the cursor (backcompat)',
        );
    });

    test('runNextChunkAndMove on an empty next chunk lands on the header (not the closing fence)', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runNextChunkAndMove');
        if (r_console_disabled) return;
        // Build a fixture whose next runnable chunk is empty. For an Rmd empty
        // chunk, `header_line + 1` would land on the closing fence, which is
        // structurally inside the chunk but a fragile cursor home. The helper
        // should fall back to the header line itself.
        const FIXTURE = [
            '```{r first}',          // 0
            'x <- 1',                // 1
            '```',                   // 2
            '',                      // 3
            '```{r empty}',          // 4  ← next chunk, empty body
            '```',                   // 5  ← closing fence
            '',                      // 6
        ].join('\n');
        const editor = await open_doc(FIXTURE, 'rmd');
        place_cursor(editor, 1); // inside "first"
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runNextChunkAndMove');
        } finally {
            term.restore();
        }
        // The empty chunk has no body to send; we still place the cursor on
        // its header rather than the closing fence.
        assert.strictEqual(
            editor.selection.active.line,
            4,
            'cursor should land on the empty chunk\'s header, not the closing fence',
        );
    });

    test('runPreviousChunk (legacy) does not move the cursor', async () => {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runPreviousChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        place_cursor(editor, 17); // inside the "second" R chunk
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runPreviousChunk');
        } finally {
            term.restore();
        }
        assert.strictEqual(
            editor.selection.active.line,
            17,
            'legacy runPreviousChunk must not move the cursor (backcompat)',
        );
    });

    test('raven.chunks.codeLens.commands controls lens count and order', async function () {
        // Skip when R-console activation is disabled — the CodeLens provider is
        // only registered alongside the run commands.
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAt');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        const config = vscode.workspace.getConfiguration();
        const previous = config.inspect<string[]>('raven.chunks.codeLens.commands')?.globalValue;
        try {
            await config.update(
                'raven.chunks.codeLens.commands',
                ['raven.runCurrentChunk', 'raven.runAllChunks', 'raven.runBelowChunks'],
                vscode.ConfigurationTarget.Global,
            );
            // RMD_FIXTURE contains three R chunks. The first two chunks carry
            // all three lenses in the configured order; the last chunk drops
            // `↧ Run Below` (gated on `requires_next_runnable`). 3 + 3 + 2 = 8.
            const lenses = await poll_for_lenses(
                editor.document.uri,
                (ls) => ls.length === 8,
            );
            const titles = lenses.map((l) => l.command?.title ?? '');
            const first_chunk_titles = titles.slice(0, 3);
            assert.ok(
                first_chunk_titles[0]?.startsWith('▷ Run Chunk'),
                `first lens should be Run Chunk, got: ${first_chunk_titles[0]}`,
            );
            assert.ok(
                first_chunk_titles[1]?.startsWith('↻ Run All'),
                `second lens should be Run All, got: ${first_chunk_titles[1]}`,
            );
            assert.ok(
                first_chunk_titles[2]?.startsWith('↧ Run Below'),
                `third lens should be Run Below, got: ${first_chunk_titles[2]}`,
            );
            assert.strictEqual(
                lenses.length,
                8,
                `expected 8 lenses (3 + 3 + 2, last chunk omits Run Below), got ${lenses.length}`,
            );
            const last_chunk_titles = titles.slice(-2);
            assert.ok(
                last_chunk_titles.every((t) => !t.startsWith('↧ Run Below')),
                `Run Below lens should be absent on the last chunk, got: ${last_chunk_titles.join(', ')}`,
            );
        } finally {
            await config.update(
                'raven.chunks.codeLens.commands',
                previous,
                vscode.ConfigurationTarget.Global,
            );
        }
    });

    test('regular Markdown shows only Run Chunk for R fences and executes its body', async function () {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAt');
        if (r_console_disabled) return;
        const editor = await open_doc(MARKDOWN_FIXTURE, 'markdown');
        const config = vscode.workspace.getConfiguration();
        const previous = config.inspect<string[]>('raven.chunks.codeLens.commands')?.globalValue;
        try {
            await config.update(
                'raven.chunks.codeLens.commands',
                ['raven.runCurrentChunk', 'raven.runAllChunks', 'raven.runBelowChunks'],
                vscode.ConfigurationTarget.Global,
            );
            const lenses = await poll_for_lenses(
                editor.document.uri,
                (ls) => ls.length === 1,
            );
            assert.strictEqual(lenses.length, 1, 'Markdown should expose one lens for its R block');
            assert.strictEqual(lenses[0].command?.title, '▷ Run Chunk');

            await dispose_any_cached_r_terminal();
            const term = stub_create_terminal();
            try {
                const command = lenses[0].command;
                assert.ok(command, 'Run Chunk lens should carry a command');
                await vscode.commands.executeCommand(command.command, ...(command.arguments ?? []));
                assert.ok(
                    term.sent.join('\n').includes('summary(cars)'),
                    `expected Markdown R block body to reach terminal, got: ${term.sent.join('\n')}`,
                );
                assert.ok(
                    !term.sent.join('\n').includes('print("not R")'),
                    'non-R Markdown block must not be sent',
                );
            } finally {
                term.restore();
            }
        } finally {
            await config.update(
                'raven.chunks.codeLens.commands',
                previous,
                vscode.ConfigurationTarget.Global,
            );
        }
    });

    test('regular Markdown does not participate in chunk navigation commands', async function () {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.goToNextChunk');
        if (r_console_disabled) return;
        const editor = await open_doc(MARKDOWN_FIXTURE, 'markdown');
        place_cursor(editor, 0);
        await execute_chunk_command(editor, 'raven.goToNextChunk');
        assert.strictEqual(
            editor.selection.active.line,
            0,
            'Markdown navigation command should leave the cursor unchanged',
        );
    });

    test('regular Markdown rejects multi-chunk and cursor-moving run commands', async function () {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAndMove');
        if (r_console_disabled) return;
        const editor = await open_doc(MARKDOWN_FIXTURE, 'markdown');
        place_cursor(editor, 3);
        await dispose_any_cached_r_terminal();
        const term = stub_create_terminal();
        try {
            await execute_chunk_command(editor, 'raven.runCurrentChunkAndMove');
            await execute_chunk_command(editor, 'raven.runAllChunks');
            assert.strictEqual(editor.selection.active.line, 3);
            assert.strictEqual(term.sent.length, 0);
        } finally {
            term.restore();
        }
    });

    test('default codeLens row shows ▷ Run Chunk ↘ Run Next Chunk ↥ Run Above', async function () {
        // Issue #280: the out-of-the-box default lens row includes the new
        // `↘ Run Next Chunk` button between `▷ Run Chunk` and `↥ Run Above`.
        // Sibling-targeted lenses are gated by their target's existence:
        // the first runnable chunk drops `↥ Run Above` (nothing above), and
        // the last runnable chunk drops `↘ Run Next Chunk` (nothing below).
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAt');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        const config = vscode.workspace.getConfiguration();
        const previous = config.inspect<string[]>('raven.chunks.codeLens.commands')?.globalValue;
        try {
            // Reset the global override so package.json's default is used.
            await config.update(
                'raven.chunks.codeLens.commands',
                undefined,
                vscode.ConfigurationTarget.Global,
            );
            // 3 runnable R chunks. Expected lens counts:
            //   first  chunk: Run Chunk + Run Next Chunk           = 2 (Run Above hidden)
            //   middle chunk: Run Chunk + Run Next Chunk + Above   = 3
            //   last   chunk: Run Chunk + Run Above                = 2 (Run Next Chunk hidden)
            // Total = 7.
            const lenses = await poll_for_lenses(
                editor.document.uri,
                (ls) => ls.length === 7,
            );
            assert.strictEqual(
                lenses.length,
                7,
                `expected 7 default lenses (2 + 3 + 2, first omits Run Above, last omits Run Next Chunk), got ${lenses.length}`,
            );
            const first_two = lenses.slice(0, 2).map((l) => l.command?.title ?? '');
            assert.ok(
                first_two[0]?.startsWith('▷ Run Chunk'),
                `default lens 1 should be Run Chunk, got: ${first_two[0]}`,
            );
            assert.ok(
                first_two[1]?.startsWith('↘ Run Next Chunk'),
                `default lens 2 should be Run Next Chunk, got: ${first_two[1]}`,
            );
            assert.ok(
                first_two.every((t) => !t.startsWith('↥ Run Above')),
                `Run Above lens should be absent on the first chunk, got: ${first_two.join(', ')}`,
            );
            const middle_three = lenses.slice(2, 5).map((l) => l.command?.title ?? '');
            assert.ok(
                middle_three.some((t) => t.startsWith('↥ Run Above')),
                `middle chunk should have Run Above, got: ${middle_three.join(', ')}`,
            );
            const last_chunk_titles = lenses.slice(-2).map((l) => l.command?.title ?? '');
            assert.ok(
                last_chunk_titles.every((t) => !(t.startsWith('→ Run Next') || t.startsWith('↘ Run Next'))),
                `Run Next lenses should be absent on the last chunk, got: ${last_chunk_titles.join(', ')}`,
            );
            assert.ok(
                last_chunk_titles.some((t) => t.startsWith('↥ Run Above')),
                `last chunk should have Run Above, got: ${last_chunk_titles.join(', ')}`,
            );
        } finally {
            await config.update(
                'raven.chunks.codeLens.commands',
                previous,
                vscode.ConfigurationTarget.Global,
            );
        }
    });

    test('Run Next/Previous lenses are gated by target availability', async function () {
        // The user-reported regression: clicking "↘ Run Next Chunk" on
        // the LAST runnable chunk surfaced a cursor-referencing toast
        // even though the click did not involve the cursor. Fix: hide
        // sibling-targeted lenses on chunks where the target chunk
        // does not exist. Symmetric for "← Run Previous" on the first.
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAt');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        const config = vscode.workspace.getConfiguration();
        const previous = config.inspect<string[]>('raven.chunks.codeLens.commands')?.globalValue;
        try {
            await config.update(
                'raven.chunks.codeLens.commands',
                [
                    'raven.runPreviousChunk',
                    'raven.runPreviousChunkAndMove',
                    'raven.runCurrentChunk',
                    'raven.runNextChunk',
                    'raven.runNextChunkAndMove',
                ],
                vscode.ConfigurationTarget.Global,
            );
            // RMD_FIXTURE has 3 R chunks. Expected lens counts:
            //   first  chunk: Current + Next + Next&Move           = 3
            //                 (Previous + Previous&Move suppressed)
            //   middle chunk: Previous + Previous&Move + Current
            //                 + Next + Next&Move                    = 5
            //   last   chunk: Previous + Previous&Move + Current   = 3
            //                 (Next + Next&Move suppressed)
            // Total = 11.
            const lenses = await poll_for_lenses(
                editor.document.uri,
                (ls) => ls.length === 11,
            );
            assert.strictEqual(
                lenses.length,
                11,
                `expected 11 lenses (3 + 5 + 3), got ${lenses.length}: `
                + lenses.map((l) => l.command?.title).join(' | '),
            );
            const titles = lenses.map((l) => l.command?.title ?? '');
            const is_run_previous = (t: string) =>
                t.startsWith('← Run Previous') || t.startsWith('↖ Run Previous');
            const is_run_next = (t: string) =>
                t.startsWith('→ Run Next') || t.startsWith('↘ Run Next');
            // First chunk: no Previous variants.
            const first_chunk = titles.slice(0, 3);
            assert.ok(
                first_chunk.every((t) => !is_run_previous(t)),
                `first chunk should not have any Run Previous lens, got: ${first_chunk.join(', ')}`,
            );
            // Last chunk: no Next variants.
            const last_chunk = titles.slice(-3);
            assert.ok(
                last_chunk.every((t) => !is_run_next(t)),
                `last chunk should not have any Run Next lens, got: ${last_chunk.join(', ')}`,
            );
            // Middle chunk has both sides.
            const middle_chunk = titles.slice(3, 8);
            assert.ok(
                middle_chunk.some(is_run_previous),
                `middle chunk should have Run Previous, got: ${middle_chunk.join(', ')}`,
            );
            assert.ok(
                middle_chunk.some(is_run_next),
                `middle chunk should have Run Next, got: ${middle_chunk.join(', ')}`,
            );
        } finally {
            await config.update(
                'raven.chunks.codeLens.commands',
                previous,
                vscode.ConfigurationTarget.Global,
            );
        }
    });

    test('Run Above lens is hidden on the topmost runnable chunk', async function () {
        // Bug report: `↥ Run Above` appeared on every chunk including the
        // first runnable one, where it has nothing to run. Treat it like
        // the sibling-targeted lenses: gate on `requires_previous_runnable`.
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAt');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        const config = vscode.workspace.getConfiguration();
        const previous = config.inspect<string[]>('raven.chunks.codeLens.commands')?.globalValue;
        try {
            await config.update(
                'raven.chunks.codeLens.commands',
                ['raven.runCurrentChunk', 'raven.runAboveChunks'],
                vscode.ConfigurationTarget.Global,
            );
            // RMD_FIXTURE has 3 runnable R chunks. Expected lens counts:
            //   first  chunk: Current             = 1  (Run Above suppressed)
            //   middle chunk: Current + Run Above = 2
            //   last   chunk: Current + Run Above = 2
            // Total = 5.
            const lenses = await poll_for_lenses(
                editor.document.uri,
                (ls) => ls.length === 5,
            );
            assert.strictEqual(
                lenses.length,
                5,
                `expected 5 lenses (1 + 2 + 2), got ${lenses.length}: `
                + lenses.map((l) => l.command?.title).join(' | '),
            );
            const titles = lenses.map((l) => l.command?.title ?? '');
            // First chunk: only Run Chunk, no Run Above.
            const first_chunk = titles.slice(0, 1);
            assert.ok(
                first_chunk.every((t) => !t.startsWith('↥ Run Above')),
                `first chunk should not have any Run Above lens, got: ${first_chunk.join(', ')}`,
            );
            // Middle and last chunks have Run Above.
            const middle_chunk = titles.slice(1, 3);
            assert.ok(
                middle_chunk.some((t) => t.startsWith('↥ Run Above')),
                `middle chunk should have Run Above, got: ${middle_chunk.join(', ')}`,
            );
            const last_chunk = titles.slice(3, 5);
            assert.ok(
                last_chunk.some((t) => t.startsWith('↥ Run Above')),
                `last chunk should have Run Above, got: ${last_chunk.join(', ')}`,
            );
        } finally {
            await config.update(
                'raven.chunks.codeLens.commands',
                previous,
                vscode.ConfigurationTarget.Global,
            );
        }
    });

    test('raven.chunks.codeLens.commands set to [] hides all lenses', async function () {
        const r_console_disabled = !(await vscode.commands.getCommands(true))
            .includes('raven.runCurrentChunkAt');
        if (r_console_disabled) return;
        const editor = await open_doc(RMD_FIXTURE, 'rmd');
        const config = vscode.workspace.getConfiguration();
        const previous = config.inspect<string[]>('raven.chunks.codeLens.commands')?.globalValue;
        try {
            await config.update(
                'raven.chunks.codeLens.commands',
                [],
                vscode.ConfigurationTarget.Global,
            );
            const lenses = await poll_for_lenses(
                editor.document.uri,
                (ls) => ls.length === 0,
            );
            assert.strictEqual(lenses.length, 0, 'empty array should hide every lens');
        } finally {
            await config.update(
                'raven.chunks.codeLens.commands',
                previous,
                vscode.ConfigurationTarget.Global,
            );
        }
    });
});
