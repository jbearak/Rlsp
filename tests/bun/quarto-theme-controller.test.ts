import { describe, expect, test } from 'bun:test';
import { githubDark } from '../../editors/vscode/src/knit/github-colors';
import type { ThemePaletteArgs } from '../../editors/vscode/src/knit/vscode-theme-palette';
import {
    QUARTO_THEME_PREFERENCE_KEY,
    QuartoThemeController,
    type QuartoThemeControllerDeps,
} from '../../editors/vscode/src/quarto/quarto-theme-controller';
import type { QuartoThemePayload } from '../../editors/vscode/src/quarto/quarto-messages';

const fakePalette = {
    background: '#101010',
    foreground: '#eeeeee',
    roles: {
        keyword: '#ff0001',
        string: '#ff0002',
        number: '#ff0003',
        comment: '#ff0004',
        function: '#ff0005',
        type: '#ff0006',
        variable: '#ff0007',
        operator: '#ff0008',
        punctuation: '#ff0009',
        constant: '#ff0010',
    },
};

function harness(overrides: Partial<QuartoThemeControllerDeps> = {}) {
    const stored = new Map<string, unknown>();
    const posted: QuartoThemePayload[] = [];
    const lines: string[] = [];
    let themeListener: (() => void) | undefined;
    let configListener: ((event: { affectsConfiguration(key: string): boolean }) => void) | undefined;
    const deps: QuartoThemeControllerDeps = {
        context: {
            globalState: {
                get: <T>(key: string) => stored.get(key) as T | undefined,
                update: async (key, value) => { stored.set(key, value); },
            },
        },
        output: { appendLine: (line) => { lines.push(line); } },
        sourceFsPath: '/work/report.qmd',
        postThemeUpdate: async (payload) => {
            posted.push(payload);
            return true;
        },
        requestThemeContext: () => true,
        activeThemeKind: () => 'dark',
        getConfiguration: <T>(key: string, fallback?: T) =>
            (key === 'workbench.colorTheme' ? 'Fake Theme' : fallback) as T,
        themeResolverInputs: () => ({
            extensions: [],
            tokenColorCustomizations: undefined,
            semanticTokenColorCustomizations: undefined,
            registry: {} as ThemePaletteArgs['registry'],
            readFile: async () => '',
            realPath: async (value) => value,
        }),
        resolvePalette: async () => ({
            ok: true,
            palette: fakePalette,
            themeId: 'Fake Theme',
            isLight: false,
            candidateFailures: [],
        }),
        resolveFonts: () => ({
            text: 'Inter, sans-serif',
            mono: 'Mono, monospace',
        }),
        sourceConfigurationScope: (sourceFsPath) => sourceFsPath,
        onDidChangeActiveColorTheme: (listener) => {
            themeListener = listener;
            return { dispose: () => undefined };
        },
        onDidChangeConfiguration: (listener) => {
            configListener = listener;
            return { dispose: () => undefined };
        },
        ...overrides,
    };
    return {
        controller: new QuartoThemeController(deps),
        stored,
        posted,
        lines,
        fireTheme: () => themeListener?.(),
        fireConfig: (keys: readonly string[]) => configListener?.({
            affectsConfiguration: (key) => keys.includes(key),
        }),
    };
}

describe('QuartoThemeController', () => {
    test('builds the payload from injected palette and font resolvers', async () => {
        let args: ThemePaletteArgs | undefined;
        const h = harness({
            resolvePalette: async (received) => {
                args = received;
                return {
                    ok: true,
                    palette: fakePalette,
                    themeId: 'Fake Theme',
                    isLight: false,
                    candidateFailures: [],
                };
            },
        });
        try {
            expect(await h.controller.push()).toBe(true);
            expect(args?.candidateThemeIds).toEqual(['Fake Theme']);
            expect(h.posted).toEqual([{
                enabled: false,
                background: fakePalette.background,
                codeBackground: fakePalette.background,
                foreground: fakePalette.foreground,
                roles: fakePalette.roles,
                fontText: 'Inter, sans-serif',
                fontMono: 'Mono, monospace',
            }]);
        } finally {
            h.controller.dispose();
        }
    });

    test('layers flat and active-theme workbench color customizations', async () => {
        const h = harness({
            resolvePalette: async () => ({
                ok: true,
                palette: fakePalette,
                themeId: 'fake-theme-id',
                themeLabel: 'Fake Theme',
                isLight: false,
                candidateFailures: [],
            }),
            getConfiguration: <T>(key: string, fallback?: T) => {
                if (key === 'workbench.colorTheme') return 'Fake Theme' as T;
                if (key === 'workbench.colorCustomizations') {
                    return {
                        'editor.background': '#202020',
                        'editor.foreground': 'not-a-color',
                        '[Fake Theme]': {
                            'editor.foreground': 'rgb(240, 240, 240)',
                        },
                        '[Other Theme]': {
                            'editor.background': '#ffffff',
                        },
                    } as T;
                }
                return fallback;
            },
        });
        try {
            await h.controller.push();
            expect(h.posted.at(-1)?.background).toBe('#202020');
            expect(h.posted.at(-1)?.foreground).toBe('rgb(240, 240, 240)');
        } finally {
            h.controller.dispose();
        }
    });

    test('uses the live VS Code text-code-block background', async () => {
        const h = harness();
        try {
            h.controller.setThemeContext('#101010', 'rgba(255, 255, 255, 0.08)');
            await new Promise((resolve) => setTimeout(resolve, 0));
            expect(h.posted.at(-1)?.codeBackground)
                .toBe('rgba(255, 255, 255, 0.08)');
        } finally {
            h.controller.dispose();
        }
    });

    test('layers combined and glob theme selectors in object key order', async () => {
        const h = harness({
            resolvePalette: async () => ({
                ok: true,
                palette: fakePalette,
                themeId: 'default-dark-plus',
                themeLabel: 'Default Dark+',
                isLight: false,
                candidateFailures: [],
            }),
            getConfiguration: <T>(key: string, fallback?: T) => {
                if (key === 'workbench.colorTheme') return 'Default Dark+' as T;
                if (key === 'workbench.colorCustomizations') {
                    return {
                        'editor.background': '#202020',
                        'editor.foreground': '#dddddd',
                        '[Default Light+][Default Dark+]': {
                            'editor.background': '#303030',
                        },
                        '[Unrelated Theme]': {
                            'editor.background': '#ffffff',
                            'editor.foreground': '#000000',
                        },
                        '[*Dark*]': {
                            'editor.foreground': 'rgb(230, 230, 230)',
                        },
                        '[Default *]': {
                            'editor.background': '#404040',
                        },
                    } as T;
                }
                return fallback;
            },
        });
        try {
            await h.controller.push();
            expect(h.posted.at(-1)?.background).toBe('#404040');
            expect(h.posted.at(-1)?.foreground).toBe('rgb(230, 230, 230)');
        } finally {
            h.controller.dispose();
        }
    });

    test('persists and broadcasts enabled state across controllers', async () => {
        const first = harness();
        const second = harness();
        try {
            await first.controller.setEnabled(true);
            expect(first.stored.get(QUARTO_THEME_PREFERENCE_KEY)).toBe(true);
            expect(first.controller.isEnabled).toBe(true);
            expect(second.controller.isEnabled).toBe(true);
            expect(first.posted.at(-1)?.enabled).toBe(true);
            expect(second.posted.at(-1)?.enabled).toBe(true);
        } finally {
            first.controller.dispose();
            second.controller.dispose();
        }
    });

    test('logs a failed preference write without rejecting setEnabled', async () => {
        const h = harness({
            context: {
                globalState: {
                    get: () => undefined,
                    update: async () => { throw new Error('memento unavailable'); },
                },
            },
        });
        try {
            await expect(h.controller.setEnabled(true)).resolves.toBeUndefined();
            expect(h.lines).toEqual([
                '[theme] Quarto VS Code theme preference failed to persist.',
            ]);
            expect(h.controller.isEnabled).toBe(true);
            await h.controller.setEnabled(false);
        } finally {
            h.controller.dispose();
        }
    });

    test('serializes rapid persistence writes so the last toggle wins', async () => {
        const calls: boolean[] = [];
        const releases: Array<() => void> = [];
        let persisted: boolean | undefined;
        const h = harness({
            context: {
                globalState: {
                    get: () => undefined,
                    update: async (_key, value) => {
                        calls.push(value as boolean);
                        await new Promise<void>((resolve) => { releases.push(resolve); });
                        persisted = value as boolean;
                    },
                },
            },
        });
        try {
            const older = h.controller.setEnabled(true);
            const newer = h.controller.setEnabled(false);
            await Promise.resolve();
            await Promise.resolve();
            expect(calls).toEqual([true]);

            releases[0]();
            for (let turn = 0; turn < 10 && calls.length < 2; turn++) {
                await Promise.resolve();
            }
            expect(calls).toEqual([true, false]);
            releases[1]();
            await Promise.all([older, newer]);

            expect(persisted).toBe(false);
            expect(h.controller.isEnabled).toBe(false);
        } finally {
            h.controller.dispose();
        }
    });

    test('drops a stale generation whose resolver finishes last', async () => {
        const releases: Array<(value: unknown) => void> = [];
        const h = harness({
            resolvePalette: () => new Promise((resolve) => { releases.push(resolve); }) as never,
        });
        try {
            const older = h.controller.push();
            const newer = h.controller.push();
            releases[1]({
                ok: true,
                palette: fakePalette,
                themeId: 'newer',
                isLight: false,
                candidateFailures: [],
            });
            expect(await newer).toBe(true);
            releases[0]({
                ok: true,
                palette: { ...fakePalette, background: '#222222' },
                themeId: 'older',
                isLight: false,
                candidateFailures: [],
            });
            expect(await older).toBe(false);
            expect(h.posted).toHaveLength(1);
            expect(h.posted[0].background).toBe('#101010');
        } finally {
            h.controller.dispose();
        }
    });

    test('falls back to GitHub palette and logs only once when resolution throws', async () => {
        const h = harness({
            resolvePalette: async () => { throw new Error('injected resolver failure'); },
        });
        try {
            await h.controller.push();
            await h.controller.push();
            expect(h.posted[0].background).toBe(githubDark.background);
            expect(h.posted[0].roles).toEqual(githubDark.roles);
            expect(h.lines).toHaveLength(1);
            expect(h.lines[0]).toContain('injected resolver failure');
        } finally {
            h.controller.dispose();
        }
    });

    test('same-kind theme listener re-requests shell context and re-pushes', async () => {
        let contextRequests = 0;
        const h = harness({
            requestThemeContext: () => {
                contextRequests++;
                return true;
            },
        });
        try {
            h.fireTheme();
            await new Promise((resolve) => setTimeout(resolve, 0));
            expect(contextRequests).toBe(1);
            expect(h.posted).toHaveLength(1);
        } finally {
            h.controller.dispose();
        }
    });

    test('a controller created during a pending preference write uses last-known state', async () => {
        let release!: () => void;
        const persistenceGate = new Promise<void>((resolve) => { release = resolve; });
        const first = harness({
            context: {
                globalState: {
                    get: () => false,
                    update: async () => { await persistenceGate; },
                },
            },
        });
        let second: ReturnType<typeof harness> | undefined;
        try {
            const enabling = first.controller.setEnabled(true);
            second = harness({
                context: {
                    globalState: {
                        get: () => false,
                        update: async () => undefined,
                    },
                },
            });
            expect(second.controller.isEnabled).toBe(true);
            release();
            await enabling;
            await first.controller.setEnabled(false);
        } finally {
            release();
            first.controller.dispose();
            second?.controller.dispose();
        }
    });
});
