import * as vscode from 'vscode';
import {
    classify_chunk_document_for_document,
    detect_chunks,
    find_chunk_at_line,
    chunks_above,
    chunks_below,
    extract_chunk_code,
    has_chunk_anchor,
    is_runnable_chunk,
    next_runnable_chunk,
    previous_runnable_chunk,
    Chunk,
} from './chunk-detector';
import { get_or_create_r_terminal } from '../send-to-r/r-terminal-manager';
import { send_code, get_send_options } from '../send-to-r/send-code';

export type RunMode =
    | 'current'
    | 'currentAndMove'
    | 'above'
    | 'all'
    | 'currentAndBelow'
    | 'below'
    | 'previous'
    | 'previousAndMove'
    | 'next'
    | 'nextAndMove';

function get_document_lines(document: vscode.TextDocument): string[] {
    const lines: string[] = [];
    for (let i = 0; i < document.lineCount; i++) {
        lines.push(document.lineAt(i).text);
    }
    return lines;
}

function chunks_for_document(document: vscode.TextDocument): Chunk[] {
    const kind = classify_chunk_document_for_document(document);
    // Fast path: skip the per-line scan when the document has no chunk anchors.
    // For a plain `.R` file with no `# %%` markers this avoids materializing the
    // line array AND running the marker regex on every keystroke.
    if (!has_chunk_anchor(document.getText(), kind)) return [];
    return detect_chunks(get_document_lines(document), kind);
}

function combined_code(lines: string[], chunks: Chunk[]): string {
    const parts: string[] = [];
    for (const c of chunks) {
        if (!is_runnable_chunk(c)) continue;
        const code = extract_chunk_code(lines, c);
        if (code.trim().length > 0) parts.push(code);
    }
    return parts.join('\n');
}

/**
 * Target terminal for a chunk run. `managed` reuses Raven's built-in R
 * terminal (creating it if necessary); `active` sends to whatever terminal
 * the user has focused right now, exactly like the `raven.terminal.*` Send
 * to R commands. The split lets the Terminal submenu offer the same chunk
 * operations as the main menu without forcing them through the managed
 * terminal — useful for `tmux`-hosted R, Docker containers, etc.
 */
type TerminalTarget = 'managed' | 'active';

async function resolve_target_terminal(
    target: TerminalTarget,
): Promise<vscode.Terminal | null> {
    if (target === 'managed') {
        return get_or_create_r_terminal();
    }
    const active = vscode.window.activeTerminal;
    if (!active) {
        vscode.window.showErrorMessage('No active terminal. Open a terminal first.');
        return null;
    }
    return active;
}

async function send_to_r(code: string, target: TerminalTarget): Promise<void> {
    if (code.trim().length === 0) return;
    try {
        const terminal = await resolve_target_terminal(target);
        if (!terminal) return;
        terminal.show(true);
        send_code(terminal, code, get_send_options());
    } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Raven: failed to send chunk to R — ${message}`);
    }
}

function find_visible_editor(uri: vscode.Uri): vscode.TextEditor | undefined {
    return vscode.window.visibleTextEditors.find(
        (e) => e.document.uri.toString() === uri.toString(),
    );
}

/**
 * Move the cursor to the first body line of `chunk` and reveal it.
 *
 * Reused by every `…AndMove` variant: `runCurrentChunkAndMove` jumps to the
 * chunk *after* the one that was run (relay-the-baton), while the issue-#280
 * `runNextChunkAndMove` / `runPreviousChunkAndMove` variants jump *into* the
 * chunk that was just run (Quarto's Run-Next-and-Move behavior). The single
 * primitive handles both — callers compute which chunk to land in.
 *
 * Empty chunks (`end_line === header_line`) have no body line — for an empty
 * Rmd chunk, `header_line + 1` is the closing fence; for an empty `# %%` cell
 * it's the next cell marker (i.e. a different cell). In both cases we fall
 * back to `header_line` itself so the cursor stays associated with this
 * chunk.
 */
function place_cursor_in_chunk(editor: vscode.TextEditor, chunk: Chunk): void {
    const has_body_line = chunk.end_line > chunk.header_line;
    const ideal_line = has_body_line ? chunk.header_line + 1 : chunk.header_line;
    const target_line = Math.min(ideal_line, editor.document.lineCount - 1);
    const pos = new vscode.Position(target_line, 0);
    editor.selection = new vscode.Selection(pos, pos);
    editor.revealRange(new vscode.Range(pos, pos));
}

function move_cursor_to_next_chunk(
    editor: vscode.TextEditor,
    chunks: Chunk[],
    current: Chunk,
): void {
    const next = chunks.find((c) => c.header_line > current.header_line && is_runnable_chunk(c));
    if (!next) return;
    place_cursor_in_chunk(editor, next);
}

async function run_chunk_at(
    mode: RunMode,
    document: vscode.TextDocument,
    cursor_line: number,
    target: TerminalTarget = 'managed',
): Promise<void> {
    const kind = classify_chunk_document_for_document(document);
    // Regular Markdown is deliberately an execution-only, one-block surface.
    // Commands are registered globally, so enforce the same boundary for
    // Command Palette and keybinding invocations as the CodeLens provider.
    if (kind === 'markdown' && mode !== 'current') return;

    // Active-terminal path: bail upfront if there is no terminal to send
    // into. Otherwise the `…AndMove` variants would still advance the
    // cursor after `send_to_r` short-circuited on the "no active terminal"
    // error, leaving the user with a moved cursor and nothing sent.
    if (target === 'active' && !vscode.window.activeTerminal) {
        vscode.window.showErrorMessage('No active terminal. Open a terminal first.');
        return;
    }

    const editor = find_visible_editor(document.uri);
    const lines = get_document_lines(document);
    const chunks = chunks_for_document(document);
    if (chunks.length === 0) {
        vscode.window.showInformationMessage('Raven: no R chunks found in this document.');
        return;
    }

    if (mode === 'all') {
        const code = combined_code(lines, chunks);
        if (code.length === 0) {
            vscode.window.showInformationMessage('Raven: no runnable R chunks to execute.');
            return;
        }
        await send_to_r(code, target);
        return;
    }

    if (mode === 'above') {
        const above = chunks_above(chunks, cursor_line);
        const code = combined_code(lines, above);
        if (code.length === 0) {
            vscode.window.showInformationMessage('Raven: no runnable chunks above the cursor.');
            return;
        }
        await send_to_r(code, target);
        return;
    }

    if (mode === 'below') {
        const below = chunks_below(chunks, cursor_line);
        const code = combined_code(lines, below);
        if (code.length === 0) {
            vscode.window.showInformationMessage('Raven: no runnable chunks below the cursor.');
            return;
        }
        await send_to_r(code, target);
        return;
    }

    if (mode === 'previous' || mode === 'previousAndMove') {
        const previous = previous_runnable_chunk(chunks, cursor_line);
        if (!previous) {
            vscode.window.showInformationMessage('Raven: no runnable chunk above the cursor.');
            return;
        }
        const code = extract_chunk_code(lines, previous);
        if (code.trim().length === 0) {
            vscode.window.showInformationMessage('Raven: previous chunk is empty.');
            if (mode === 'previousAndMove' && editor) place_cursor_in_chunk(editor, previous);
            return;
        }
        await send_to_r(code, target);
        if (mode === 'previousAndMove' && editor) place_cursor_in_chunk(editor, previous);
        return;
    }

    if (mode === 'next' || mode === 'nextAndMove') {
        const next = next_runnable_chunk(chunks, cursor_line);
        if (!next) {
            vscode.window.showInformationMessage('Raven: no runnable chunk below the cursor.');
            return;
        }
        const code = extract_chunk_code(lines, next);
        if (code.trim().length === 0) {
            vscode.window.showInformationMessage('Raven: next chunk is empty.');
            if (mode === 'nextAndMove' && editor) place_cursor_in_chunk(editor, next);
            return;
        }
        await send_to_r(code, target);
        if (mode === 'nextAndMove' && editor) place_cursor_in_chunk(editor, next);
        return;
    }

    const current = find_chunk_at_line(chunks, cursor_line);
    if (!current) {
        vscode.window.showInformationMessage(
            'Raven: cursor is not inside an R chunk. Place the cursor inside a ```{r} block or after a `# %%` marker.'
        );
        return;
    }
    if (!is_runnable_chunk(current)) {
        vscode.window.showInformationMessage(
            `Raven: current chunk language is "${current.language}" — only "r" chunks can be sent to the R console.`
        );
        return;
    }

    if (mode === 'currentAndBelow') {
        const below = chunks_below(chunks, current.header_line);
        const code = combined_code(lines, [current, ...below]);
        if (code.length === 0) {
            vscode.window.showInformationMessage('Raven: current chunk and chunks below are empty.');
            return;
        }
        await send_to_r(code, target);
        return;
    }

    const code = extract_chunk_code(lines, current);
    if (code.trim().length === 0) {
        vscode.window.showInformationMessage('Raven: current chunk is empty.');
        if (mode === 'currentAndMove' && editor) move_cursor_to_next_chunk(editor, chunks, current);
        return;
    }
    await send_to_r(code, target);
    if (mode === 'currentAndMove' && editor) move_cursor_to_next_chunk(editor, chunks, current);
}

async function run_chunk(mode: RunMode, target: TerminalTarget = 'managed'): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) return;
    await run_chunk_at(mode, editor.document, editor.selection.active.line, target);
}

async function run_chunk_at_command(
    mode: RunMode,
    uri_or_arg: unknown,
    line_arg: unknown,
): Promise<void> {
    const uri = uri_or_arg instanceof vscode.Uri ? uri_or_arg : null;
    const line = typeof line_arg === 'number' ? line_arg : null;
    if (uri === null || line === null) {
        // Invoked without arguments (e.g. directly from the command palette).
        // Fall back to the active editor's cursor.
        await run_chunk(mode);
        return;
    }
    let document: vscode.TextDocument;
    try {
        document = await vscode.workspace.openTextDocument(uri);
    } catch (err) {
        // Stale CodeLens: refuse to silently run a different chunk. Surface the
        // failure so the user knows the click didn't take effect.
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(
            `Raven: could not open chunk document (${message}). Try reopening the file.`
        );
        return;
    }
    await run_chunk_at(mode, document, line);
}

/**
 * Run commands a user can list in `raven.chunks.codeLens.commands` to choose
 * which CodeLens buttons appear on chunk headers and in what order. Each entry
 * maps the user-facing command id (the one declared in `package.json` and
 * invokable from the command palette) to the positional `*At` variant the
 * lens click should dispatch — so clicking a lens always targets the chunk it
 * is attached to, regardless of where the cursor sits.
 *
 * `eval_aware` lenses append a `(eval = FALSE)` suffix to their title when
 * the chunk header sets `eval = FALSE`, matching the existing "Run Chunk"
 * behavior. Multi-chunk / sibling-chunk lenses don't add the suffix: for
 * `above` / `below` / `previous` / `previousAndMove` / `next` / `nextAndMove`
 * / `all` the chunk under the lens isn't even part of the execution; for
 * `currentAndBelow` it is, but the suffix would still be misleading because
 * the chunks after it would still run regardless of this one's `eval` flag,
 * so the lens's title shouldn't make any single chunk's eval state look
 * load-bearing.
 */
export interface ChunkLensCommand {
    /** Command id dispatched by the CodeLens click (positional variant). */
    readonly positional_id: string;
    /** Lens button label. */
    readonly title: string;
    /** Hover tooltip. */
    readonly tooltip: string;
    /** Whether to append a `(eval = FALSE)` suffix when the chunk skips eval. */
    readonly eval_aware: boolean;
    /**
     * Optional gate. When set, the CodeLens provider suppresses the lens
     * on chunks for which the gate would always fail at click time —
     * e.g. "Run Next" on the last runnable chunk would have nowhere to
     * go, so we drop the button rather than surface a confusing
     * "no runnable chunk below the cursor" toast that talks about a
     * cursor the user didn't use.
     */
    readonly gate?: 'requires_next_runnable' | 'requires_previous_runnable';
}

export const CHUNK_LENS_COMMANDS: Readonly<Record<string, ChunkLensCommand>> = Object.freeze({
    'raven.runCurrentChunk': {
        positional_id: 'raven.runCurrentChunkAt',
        title: '▷ Run Chunk',
        tooltip: 'Run this chunk in the R console',
        eval_aware: true,
    },
    'raven.runCurrentChunkAndMove': {
        positional_id: 'raven.runCurrentChunkAndMoveAt',
        title: '▷⇣ Run & Move',
        tooltip: 'Run this chunk, then move the cursor into the next R chunk',
        eval_aware: true,
    },
    'raven.runAboveChunks': {
        positional_id: 'raven.runAboveChunksAt',
        title: '↥ Run Above',
        tooltip: 'Run every R chunk above this one',
        eval_aware: false,
        gate: 'requires_previous_runnable',
    },
    'raven.runCurrentAndBelowChunks': {
        positional_id: 'raven.runCurrentAndBelowChunksAt',
        title: '▷↓ Run Current and Below',
        tooltip: 'Run this chunk and every R chunk after it',
        eval_aware: false,
    },
    'raven.runBelowChunks': {
        positional_id: 'raven.runBelowChunksAt',
        title: '↧ Run Below',
        tooltip: 'Run every R chunk below this one',
        eval_aware: false,
        gate: 'requires_next_runnable',
    },
    'raven.runPreviousChunk': {
        positional_id: 'raven.runPreviousChunkAt',
        title: '← Run Previous',
        tooltip: 'Run the R chunk immediately above this one',
        eval_aware: false,
        gate: 'requires_previous_runnable',
    },
    'raven.runPreviousChunkAndMove': {
        positional_id: 'raven.runPreviousChunkAndMoveAt',
        title: '↖ Run Previous Chunk',
        tooltip: 'Run the R chunk immediately above this one, then move the cursor into it',
        eval_aware: false,
        gate: 'requires_previous_runnable',
    },
    'raven.runNextChunk': {
        positional_id: 'raven.runNextChunkAt',
        title: '→ Run Next',
        tooltip: 'Run the R chunk immediately below this one',
        eval_aware: false,
        gate: 'requires_next_runnable',
    },
    'raven.runNextChunkAndMove': {
        positional_id: 'raven.runNextChunkAndMoveAt',
        title: '↘ Run Next Chunk',
        tooltip: 'Run the R chunk immediately below this one, then move the cursor into it',
        eval_aware: false,
        gate: 'requires_next_runnable',
    },
    'raven.runAllChunks': {
        positional_id: 'raven.runAllChunksAt',
        title: '↻ Run All',
        tooltip: 'Run every R chunk in the document',
        eval_aware: false,
    },
});

export function register_chunk_commands(context: vscode.ExtensionContext): void {
    const handlers: Array<[string, RunMode]> = [
        ['raven.runCurrentChunk', 'current'],
        ['raven.runCurrentChunkAndMove', 'currentAndMove'],
        ['raven.runAboveChunks', 'above'],
        ['raven.runAllChunks', 'all'],
        ['raven.runCurrentAndBelowChunks', 'currentAndBelow'],
        ['raven.runBelowChunks', 'below'],
        ['raven.runPreviousChunk', 'previous'],
        ['raven.runPreviousChunkAndMove', 'previousAndMove'],
        ['raven.runNextChunk', 'next'],
        ['raven.runNextChunkAndMove', 'nextAndMove'],
    ];
    for (const [id, mode] of handlers) {
        context.subscriptions.push(
            vscode.commands.registerCommand(id, () => run_chunk(mode, 'managed'))
        );
    }

    // Terminal-mode counterparts: send the same chunk payload to the
    // active terminal instead of the managed R terminal, mirroring the
    // raven.terminal.runLineOrSelection / runUpwardLines / … family.
    const terminal_handlers: Array<[string, RunMode]> = [
        ['raven.terminal.runCurrentChunk', 'current'],
        ['raven.terminal.runCurrentChunkAndMove', 'currentAndMove'],
        ['raven.terminal.runAboveChunks', 'above'],
        ['raven.terminal.runAllChunks', 'all'],
        ['raven.terminal.runBelowChunks', 'below'],
    ];
    for (const [id, mode] of terminal_handlers) {
        context.subscriptions.push(
            vscode.commands.registerCommand(id, () => run_chunk(mode, 'active'))
        );
    }

    // Positional variants used by CodeLens (header line is known up-front).
    const positional: Array<[string, RunMode]> = [
        ['raven.runCurrentChunkAt', 'current'],
        ['raven.runCurrentChunkAndMoveAt', 'currentAndMove'],
        ['raven.runAboveChunksAt', 'above'],
        ['raven.runAllChunksAt', 'all'],
        ['raven.runCurrentAndBelowChunksAt', 'currentAndBelow'],
        ['raven.runBelowChunksAt', 'below'],
        ['raven.runPreviousChunkAt', 'previous'],
        ['raven.runPreviousChunkAndMoveAt', 'previousAndMove'],
        ['raven.runNextChunkAt', 'next'],
        ['raven.runNextChunkAndMoveAt', 'nextAndMove'],
    ];
    for (const [id, mode] of positional) {
        context.subscriptions.push(
            vscode.commands.registerCommand(id, (uri: unknown, line: unknown) =>
                run_chunk_at_command(mode, uri, line)
            ),
        );
    }
}

export { chunks_for_document, get_document_lines };
