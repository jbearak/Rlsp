import * as vscode from 'vscode';
import { chunks_for_document } from './chunk-commands';
import {
    Chunk,
    classify_chunk_document_for_document,
    find_chunk_at_line,
} from './chunk-detector';

function supports_chunk_navigation(document: vscode.TextDocument): boolean {
    // Regular Markdown intentionally gets only the execution CodeLens. Keep
    // the globally registered palette commands from turning it into another
    // chunk-authoring surface.
    return classify_chunk_document_for_document(document) !== 'markdown';
}

function move_to_line(editor: vscode.TextEditor, line: number): void {
    const safe = Math.max(0, Math.min(line, editor.document.lineCount - 1));
    const text = editor.document.lineAt(safe).text;
    const column = Math.min(editor.selection.active.character, text.length);
    const pos = new vscode.Position(safe, column);
    editor.selection = new vscode.Selection(pos, pos);
    editor.revealRange(new vscode.Range(pos, pos));
}

function go_to_next(): void {
    const editor = vscode.window.activeTextEditor;
    if (!editor) return;
    if (!supports_chunk_navigation(editor.document)) return;
    const chunks = chunks_for_document(editor.document);
    if (chunks.length === 0) return;
    const cursor = editor.selection.active.line;
    const next = chunks.find((c) => c.header_line > cursor);
    if (!next) {
        vscode.window.setStatusBarMessage('Raven: already at the last chunk.', 2000);
        return;
    }
    // Place the cursor one line below the header (i.e. inside the chunk body) when possible.
    const target = next.header_line + 1 <= next.end_line ? next.header_line + 1 : next.header_line;
    move_to_line(editor, target);
}

function go_to_previous(): void {
    const editor = vscode.window.activeTextEditor;
    if (!editor) return;
    if (!supports_chunk_navigation(editor.document)) return;
    const chunks = chunks_for_document(editor.document);
    if (chunks.length === 0) return;
    const cursor = editor.selection.active.line;
    // Find the last chunk whose header is strictly above the cursor's current header.
    const current = find_chunk_at_line(chunks, cursor);
    const reference_header = current ? current.header_line : cursor;
    let prev: Chunk | null = null;
    for (const c of chunks) {
        if (c.header_line < reference_header) prev = c;
        else break;
    }
    if (!prev) {
        vscode.window.setStatusBarMessage('Raven: already at the first chunk.', 2000);
        return;
    }
    const target = prev.header_line + 1 <= prev.end_line ? prev.header_line + 1 : prev.header_line;
    move_to_line(editor, target);
}

function select_current(): void {
    const editor = vscode.window.activeTextEditor;
    if (!editor) return;
    if (!supports_chunk_navigation(editor.document)) return;
    const chunks = chunks_for_document(editor.document);
    if (chunks.length === 0) return;
    const cursor = editor.selection.active.line;
    const current = find_chunk_at_line(chunks, cursor);
    if (!current) {
        vscode.window.setStatusBarMessage(
            'Raven: cursor is not inside a chunk.',
            2000,
        );
        return;
    }
    // Empty chunk (no body lines): collapse the cursor to the end of the header
    // rather than pretending we selected content. This avoids accidentally
    // capturing the closing fence (Rmd) or cell-marker line (.R).
    if (current.end_line <= current.header_line) {
        const header_text = editor.document.lineAt(current.header_line).text;
        const pos = new vscode.Position(current.header_line, header_text.length);
        editor.selection = new vscode.Selection(pos, pos);
        editor.revealRange(new vscode.Range(pos, pos));
        return;
    }
    const start = new vscode.Position(current.header_line + 1, 0);
    const end_text = editor.document.lineAt(current.end_line).text;
    const end = new vscode.Position(current.end_line, end_text.length);
    editor.selection = new vscode.Selection(start, end);
    editor.revealRange(new vscode.Range(start, end));
}

export function register_chunk_navigation(context: vscode.ExtensionContext): void {
    context.subscriptions.push(
        vscode.commands.registerCommand('raven.goToNextChunk', go_to_next),
        vscode.commands.registerCommand('raven.goToPreviousChunk', go_to_previous),
        vscode.commands.registerCommand('raven.selectCurrentChunk', select_current),
    );
}
