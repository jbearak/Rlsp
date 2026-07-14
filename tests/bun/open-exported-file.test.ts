import { describe, it, expect, mock, beforeEach } from 'bun:test';
import { openExportedFile, type ExportFormat } from '../../editors/vscode/src/knit/open-exported-file';

const state: {
    remoteName: string | undefined;
    infoMessageLabels: string[];
    infoMessageResponse: string | undefined;
    warningMessages: string[];
    executedCommands: Array<{ id: string; args: unknown[] }>;
    executeCommandImpl: ((id: string, ...args: unknown[]) => unknown) | undefined;
    openExternalResult: boolean;
    openExternalImpl: ((uri: unknown) => Promise<boolean>) | undefined;
    outputLines: string[];
} = {
    remoteName: undefined,
    infoMessageLabels: [],
    infoMessageResponse: undefined,
    warningMessages: [],
    executedCommands: [],
    executeCommandImpl: undefined,
    openExternalResult: true,
    openExternalImpl: undefined,
    outputLines: [],
};

mock.module('vscode', () => ({
    Uri: {
        file: (fsPath: string) => ({ fsPath, path: fsPath, scheme: 'file', toString: () => `file://${fsPath}` }),
    },
    commands: {
        executeCommand: async (id: string, ...args: unknown[]) => {
            state.executedCommands.push({ id, args });
            if (state.executeCommandImpl) return state.executeCommandImpl(id, ...args);
            return undefined;
        },
    },
    env: {
        get remoteName() {
            return state.remoteName;
        },
        openExternal: async (uri: unknown) => {
            if (state.openExternalImpl) return state.openExternalImpl(uri);
            return state.openExternalResult;
        },
    },
    window: {
        showInformationMessage: async (_message: string, ...labels: string[]) => {
            state.infoMessageLabels.push(...labels);
            return state.infoMessageResponse;
        },
        showWarningMessage: async (message: string) => {
            state.warningMessages.push(message);
            return undefined;
        },
    },
}));

function outputChannel() {
    return {
        append() {},
        appendLine: (line: string) => state.outputLines.push(line),
        show() {},
    };
}

function fileUri(fsPath: string) {
    return { fsPath, path: fsPath, scheme: 'file', toString: () => `file://${fsPath}` };
}

beforeEach(() => {
    state.remoteName = undefined;
    state.infoMessageLabels = [];
    state.infoMessageResponse = undefined;
    state.warningMessages = [];
    state.executedCommands = [];
    state.executeCommandImpl = undefined;
    state.openExternalResult = true;
    state.openExternalImpl = undefined;
    state.outputLines = [];
});

describe('openExportedFile — local workspace', () => {
    it.each<[ExportFormat, string]>([
        ['html', 'Open in Browser'],
        ['pdf', 'Open PDF'],
        ['docx', 'Open in Word'],
    ])(
        'offers "%s → %s" as the primary button',
        async (format: ExportFormat, expectedPrimary: string) => {
            state.remoteName = undefined;
            state.infoMessageResponse = undefined;
            await openExportedFile(fileUri('/tmp/report.out') as never, format, outputChannel() as never);
            expect(state.infoMessageLabels).toEqual([expectedPrimary, 'Open in Editor']);
        },
    );

    it('does not include "Download" in any local label set', async () => {
        for (const format of ['html', 'pdf', 'docx'] as ExportFormat[]) {
            state.remoteName = undefined;
            state.infoMessageLabels = [];
            state.infoMessageResponse = undefined;
            await openExportedFile(fileUri('/tmp/report.out') as never, format, outputChannel() as never);
            expect(state.infoMessageLabels).not.toContain('Download');
        }
    });

    it('primary "Open in …" calls vscode.env.openExternal with the saved URI', async () => {
        state.remoteName = undefined;
        state.infoMessageResponse = 'Open PDF';
        const calls: unknown[] = [];
        state.openExternalImpl = async (uri) => {
            calls.push(uri);
            return true;
        };
        await openExportedFile(fileUri('/tmp/report.pdf') as never, 'pdf', outputChannel() as never);
        expect(calls).toHaveLength(1);
        expect((calls[0] as { fsPath: string }).fsPath).toBe('/tmp/report.pdf');
        expect(state.executedCommands).toEqual([]);
        expect(state.warningMessages).toEqual([]);
    });

    it('warns when openExternal returns false', async () => {
        state.remoteName = undefined;
        state.infoMessageResponse = 'Open in Browser';
        state.openExternalResult = false;
        await openExportedFile(fileUri('/tmp/report.html') as never, 'html', outputChannel() as never);
        expect(state.warningMessages).toHaveLength(1);
        expect(state.warningMessages[0]).toContain('Open in Browser');
        expect(state.warningMessages[0]).toContain('Raven: Knit output channel');
    });

    it('does nothing when the user dismisses the toast', async () => {
        state.remoteName = undefined;
        state.infoMessageResponse = undefined;
        await openExportedFile(fileUri('/tmp/report.html') as never, 'html', outputChannel() as never);
        expect(state.executedCommands).toEqual([]);
        expect(state.warningMessages).toEqual([]);
    });
});

describe('openExportedFile — Open in Editor', () => {
    it.each<ExportFormat>(['html', 'pdf', 'docx'])(
        'invokes vscode.open with the saved URI for format=%s (local)',
        async (format: ExportFormat) => {
            state.remoteName = undefined;
            state.infoMessageResponse = 'Open in Editor';
            await openExportedFile(fileUri('/tmp/report.out') as never, format, outputChannel() as never);
            expect(state.executedCommands).toHaveLength(1);
            expect(state.executedCommands[0].id).toBe('vscode.open');
            expect((state.executedCommands[0].args[0] as { fsPath: string }).fsPath).toBe('/tmp/report.out');
        },
    );

    it('invokes vscode.open in a remote workspace too', async () => {
        state.remoteName = 'ssh-remote';
        state.infoMessageResponse = 'Open in Editor';
        await openExportedFile(fileUri('/home/user/report.html') as never, 'html', outputChannel() as never);
        expect(state.executedCommands).toHaveLength(1);
        expect(state.executedCommands[0].id).toBe('vscode.open');
    });

    it('warns when vscode.open throws', async () => {
        state.remoteName = undefined;
        state.infoMessageResponse = 'Open in Editor';
        state.executeCommandImpl = (id: string) => {
            if (id === 'vscode.open') throw new Error('boom');
            return undefined;
        };
        await openExportedFile(fileUri('/tmp/report.html') as never, 'html', outputChannel() as never);
        expect(state.warningMessages).toHaveLength(1);
        expect(state.warningMessages[0]).toContain('Open in Editor');
    });
});

describe('openExportedFile — remote workspace', () => {
    it.each<string>([
        'ssh-remote',
        'dev-container',
        'attached-container',
        'wsl',
        'codespaces',
    ])('swaps the primary button to "Download" when remoteName=%s', async (remoteName: string) => {
        state.remoteName = remoteName;
        state.infoMessageResponse = undefined;
        for (const format of ['html', 'pdf', 'docx'] as ExportFormat[]) {
            state.infoMessageLabels = [];
            await openExportedFile(fileUri('/home/user/report.out') as never, format, outputChannel() as never);
            expect(state.infoMessageLabels).toEqual(['Download', 'Open in Editor']);
        }
    });

    it('Download seeds the explorer selection then invokes explorer.download', async () => {
        // `explorer.download` reads the explorer's current selection
        // instead of accepting a URI argument, so we expect a
        // `revealInExplorer(savedUri)` call to fire first, then a bare
        // `explorer.download` with no args. The reveal must complete
        // (i.e. resolve) before the download runs.
        state.remoteName = 'ssh-remote';
        state.infoMessageResponse = 'Download';
        await openExportedFile(fileUri('/home/user/report.pdf') as never, 'pdf', outputChannel() as never);
        expect(state.executedCommands).toHaveLength(2);
        expect(state.executedCommands[0].id).toBe('revealInExplorer');
        expect((state.executedCommands[0].args[0] as { fsPath: string }).fsPath).toBe('/home/user/report.pdf');
        expect(state.executedCommands[1].id).toBe('explorer.download');
        expect(state.executedCommands[1].args).toEqual([]);
    });

    it('does not run explorer.download if revealInExplorer throws', async () => {
        state.remoteName = 'ssh-remote';
        state.infoMessageResponse = 'Download';
        state.executeCommandImpl = (id: string) => {
            if (id === 'revealInExplorer') throw new Error('no such command');
            return undefined;
        };
        await openExportedFile(fileUri('/home/user/report.docx') as never, 'docx', outputChannel() as never);
        expect(state.executedCommands.map((c) => c.id)).toEqual(['revealInExplorer']);
        expect(state.warningMessages).toHaveLength(1);
        expect(state.warningMessages[0]).toContain('Download');
    });

    it('does not invoke openExternal when Download is chosen', async () => {
        state.remoteName = 'ssh-remote';
        state.infoMessageResponse = 'Download';
        let openExternalCalls = 0;
        state.openExternalImpl = async () => {
            openExternalCalls += 1;
            return true;
        };
        await openExportedFile(fileUri('/home/user/report.docx') as never, 'docx', outputChannel() as never);
        expect(openExternalCalls).toBe(0);
    });

    it('warns when explorer.download throws', async () => {
        state.remoteName = 'ssh-remote';
        state.infoMessageResponse = 'Download';
        state.executeCommandImpl = (id: string) => {
            if (id === 'explorer.download') throw new Error('no such command');
            return undefined;
        };
        await openExportedFile(fileUri('/home/user/report.html') as never, 'html', outputChannel() as never);
        expect(state.warningMessages).toHaveLength(1);
        expect(state.warningMessages[0]).toContain('Download');
    });

    it('treats remoteName="" as local', async () => {
        state.remoteName = '';
        await openExportedFile(fileUri('/tmp/report.html') as never, 'html', outputChannel() as never);
        expect(state.infoMessageLabels).toEqual(['Open in Browser', 'Open in Editor']);
    });
});

describe('openExportedFile — caller output-channel label', () => {
    it('uses the custom label when external open is unavailable', async () => {
        state.infoMessageResponse = 'Open in Browser';
        state.openExternalResult = false;

        await openExportedFile(
            fileUri('/tmp/report.html') as never,
            'html',
            outputChannel() as never,
            'Raven: Quarto',
        );

        expect(state.warningMessages[0]).toContain('Raven: Quarto output channel');
        expect(state.warningMessages[0]).not.toContain('Raven: Knit output channel');
    });

    it('uses the custom label when editor open fails', async () => {
        state.infoMessageResponse = 'Open in Editor';
        state.executeCommandImpl = (id: string) => {
            if (id === 'vscode.open') throw new Error('boom');
            return undefined;
        };

        await openExportedFile(
            fileUri('/tmp/report.pdf') as never,
            'pdf',
            outputChannel() as never,
            'Raven: Quarto',
        );

        expect(state.warningMessages[0]).toContain('Raven: Quarto output channel');
    });

    it('uses the custom label when remote download fails', async () => {
        state.remoteName = 'ssh-remote';
        state.infoMessageResponse = 'Download';
        state.executeCommandImpl = (id: string) => {
            if (id === 'explorer.download') throw new Error('boom');
            return undefined;
        };

        await openExportedFile(
            fileUri('/tmp/report.docx') as never,
            'docx',
            outputChannel() as never,
            'Raven: Quarto',
        );

        expect(state.warningMessages[0]).toContain('Raven: Quarto output channel');
    });
});
