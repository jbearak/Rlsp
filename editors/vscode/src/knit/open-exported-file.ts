/**
 * Shared helper for the "Saved …" toast shown after a successful
 * Pandoc export. The toast offers up to two actions:
 *
 *   - Primary, format-specific external-open button:
 *       html → "Open in Browser", pdf → "Open PDF", docx → "Open in Word"
 *     In a remote workspace (`vscode.env.remoteName` set — Remote SSH,
 *     Dev Containers, WSL, Codespaces, etc.) this is replaced with
 *     "Download", which drives VS Code's built-in `explorer.download`
 *     command (the same one the file explorer's right-click "Download…"
 *     uses) to stream the file from the remote machine to a local path
 *     the user picks. `explorer.download` operates on the explorer's
 *     current selection rather than accepting a URI argument, so we
 *     `revealInExplorer` the saved file first to seed the selection.
 *     The OS default handlers behind `Open in …` route through the
 *     extension-host machine in remote workspaces and so wouldn't reach
 *     the user's local apps.
 *
 *   - Secondary, format-agnostic "Open in Editor" button that runs
 *     `vscode.commands.executeCommand('vscode.open', uri)`. Useful when
 *     the user doesn't want to leave the editor, and as a fallback
 *     channel in remote workspaces (and in editor forks like Positron,
 *     Cursor, or VSCodium — hence "Open in Editor", not "Open in VS
 *     Code"). The actual viewing experience for PDF/DOCX depends on
 *     whichever default editor / extension the host has registered for
 *     that file type.
 *
 * Every action funnels through a small fallback: if the underlying VS Code
 * API throws or returns false we log the file path and surface a warning toast
 * naming the caller's output channel. Knit callers omit `channelLabel` and
 * retain the `Raven: Knit` default; Quarto supplies `Raven: Quarto`.
 */

import * as path from 'path';
import * as vscode from 'vscode';

export type ExportFormat = 'html' | 'pdf' | 'docx';

const OPEN_LABELS: Record<ExportFormat, string> = {
    html: 'Open in Browser',
    pdf: 'Open PDF',
    docx: 'Open in Word',
};
const DOWNLOAD_LABEL = 'Download';
const OPEN_IN_EDITOR_LABEL = 'Open in Editor';

export async function openExportedFile(
    savedUri: vscode.Uri,
    format: ExportFormat,
    output: vscode.OutputChannel,
    channelLabel: string = 'Raven: Knit',
): Promise<void> {
    const remote = isRemoteWorkspace();
    const primaryLabel = remote ? DOWNLOAD_LABEL : OPEN_LABELS[format];

    const action = await vscode.window.showInformationMessage(
        `Saved ${path.basename(savedUri.fsPath)}`,
        primaryLabel,
        OPEN_IN_EDITOR_LABEL,
    );
    if (action === undefined) return;

    if (action === DOWNLOAD_LABEL) {
        await downloadFile(savedUri, output, channelLabel);
        return;
    }
    if (action === OPEN_IN_EDITOR_LABEL) {
        await openInEditor(savedUri, output, channelLabel);
        return;
    }
    await openExternal(savedUri, primaryLabel, output, channelLabel);
}

function isRemoteWorkspace(): boolean {
    const name = vscode.env.remoteName;
    return typeof name === 'string' && name.length > 0;
}

async function openExternal(
    savedUri: vscode.Uri,
    label: string,
    output: vscode.OutputChannel,
    channelLabel: string,
): Promise<void> {
    let opened = false;
    try {
        opened = await vscode.env.openExternal(savedUri);
    } catch (err) {
        output.appendLine(
            `[Export] openExternal threw: ${err instanceof Error ? err.message : String(err)}`,
        );
    }
    if (opened) return;
    output.appendLine(`[Export] file:// did not open. Output is at: ${savedUri.fsPath}`);
    void vscode.window.showWarningMessage(
        `${label} is not available for this workspace. ` +
        `The file path has been written to the ${channelLabel} output channel.`,
    );
}

async function openInEditor(
    savedUri: vscode.Uri,
    output: vscode.OutputChannel,
    channelLabel: string,
): Promise<void> {
    try {
        await vscode.commands.executeCommand('vscode.open', savedUri);
    } catch (err) {
        output.appendLine(
            `[Export] vscode.open threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        output.appendLine(`[Export] Output is at: ${savedUri.fsPath}`);
        void vscode.window.showWarningMessage(
            `${OPEN_IN_EDITOR_LABEL} failed. ` +
            `The file path has been written to the ${channelLabel} output channel.`,
        );
    }
}

async function downloadFile(
    savedUri: vscode.Uri,
    output: vscode.OutputChannel,
    channelLabel: string,
): Promise<void> {
    try {
        // `explorer.download` reads the explorer's current selection
        // instead of accepting a URI argument, so we seed the selection
        // via `revealInExplorer` first. Awaiting the reveal ensures the
        // selection is in place before download fires.
        await vscode.commands.executeCommand('revealInExplorer', savedUri);
        await vscode.commands.executeCommand('explorer.download');
    } catch (err) {
        output.appendLine(
            `[Export] download command threw: ${err instanceof Error ? err.message : String(err)}`,
        );
        output.appendLine(`[Export] Output is at: ${savedUri.fsPath}`);
        void vscode.window.showWarningMessage(
            `${DOWNLOAD_LABEL} is not available for this workspace. ` +
            `The file path has been written to the ${channelLabel} output channel.`,
        );
    }
}
