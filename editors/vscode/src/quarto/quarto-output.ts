/**
 * Non-throwing facade for the activation-owned Quarto output channel.
 *
 * Bounded deactivation may abandon a command continuation after disposing the
 * underlying VS Code channel. Every operation therefore becomes a no-op after
 * facade disposal and defensively swallows host errors during the disposal
 * race.
 */

import type * as vscode from 'vscode';

export function createSafeQuartoOutputChannel(
    raw: vscode.OutputChannel,
): vscode.OutputChannel {
    let disposed = false;
    const invoke = (operation: () => void): void => {
        if (disposed) return;
        try { operation(); } catch { /* channel may be disposing */ }
    };
    return {
        name: raw.name,
        append: (value) => invoke(() => raw.append(value)),
        appendLine: (value) => invoke(() => raw.appendLine(value)),
        replace: (value) => invoke(() => raw.replace(value)),
        clear: () => invoke(() => raw.clear()),
        show: (columnOrPreserveFocus?: vscode.ViewColumn | boolean, preserveFocus?: boolean) => {
            invoke(() => {
                if (typeof columnOrPreserveFocus === 'boolean') {
                    raw.show(columnOrPreserveFocus);
                } else {
                    raw.show(columnOrPreserveFocus, preserveFocus);
                }
            });
        },
        hide: () => invoke(() => raw.hide()),
        dispose: () => {
            if (disposed) return;
            disposed = true;
            try { raw.dispose(); } catch { /* already disposed */ }
        },
    };
}
