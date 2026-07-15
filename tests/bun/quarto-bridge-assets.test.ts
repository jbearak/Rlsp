import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import {
    QUARTO_CSS_COLOR_PATTERN_SOURCE,
    QUARTO_SANITIZED_FONT_PATTERN_SOURCE,
    QUARTO_THEME_ROLE_KEYS,
    RAVEN_QUARTO_THEME_MESSAGE_KEYS,
} from '../../editors/vscode/src/quarto/quarto-messages';

const BRIDGE_DIR = join(
    import.meta.dir,
    '..',
    '..',
    'editors',
    'vscode',
    'src',
    'quarto',
    'bridge',
);

describe('packaged Quarto theme bridge sources', () => {
    test('keeps inline protocol constants exactly aligned with the shared protocol', () => {
        const source = readFileSync(join(BRIDGE_DIR, 'bridge.js'), 'utf8');
        expect(readArrayLiteral(source, 'roleKeys')).toEqual([...QUARTO_THEME_ROLE_KEYS]);
        expect(readArrayLiteral(source, 'payloadKeys')).toEqual([
            ...RAVEN_QUARTO_THEME_MESSAGE_KEYS,
        ]);
        expect(readRegexLiteral(source, 'colorPattern').source)
            .toBe(QUARTO_CSS_COLOR_PATTERN_SOURCE);
        expect(readRegexLiteral(source, 'fontPattern').source)
            .toBe(QUARTO_SANITIZED_FONT_PATTERN_SOURCE);
    });

    test('installs its parent listener before posting the initial ready handshake', () => {
        const source = readFileSync(join(BRIDGE_DIR, 'bridge.js'), 'utf8');
        const listener = source.indexOf("window.addEventListener('message'");
        const initialReady = source.lastIndexOf('postReady();');
        expect(listener).toBeGreaterThanOrEqual(0);
        expect(initialReady).toBeGreaterThan(listener);
        expect(source).toContain('event.source !== window.parent');
        expect(source).toContain("type: 'raven-quarto-theme-ready'");
        expect(source).toContain("value.type === 'raven-quarto-theme-ping'");
    });

    test('answers an exact ping without applying a theme', () => {
        const source = readFileSync(join(BRIDGE_DIR, 'bridge.js'), 'utf8');
        const parent = { postMessageCalls: [] as unknown[] };
        let listener!: (event: { source: unknown; data: unknown }) => void;
        const styleCalls: unknown[] = [];
        const window = {
            parent: {
                postMessage: (message: unknown) => parent.postMessageCalls.push(message),
            },
            addEventListener: (_type: string, callback: typeof listener) => {
                listener = callback;
            },
        };
        const document = {
            documentElement: {
                style: { setProperty: (...args: unknown[]) => styleCalls.push(args) },
                classList: { toggle: (...args: unknown[]) => styleCalls.push(args) },
            },
        };
        new Function('window', 'document', source)(window, document);
        const initialReadyCount = parent.postMessageCalls.length;

        listener({
            source: window.parent,
            data: { type: 'raven-quarto-theme-ping' },
        });

        expect(parent.postMessageCalls).toHaveLength(initialReadyCount + 1);
        expect(parent.postMessageCalls.at(-1)).toEqual({
            type: 'raven-quarto-theme-ready',
        });
        expect(styleCalls).toEqual([]);
    });

    test('sets every bridge CSS variable', () => {
        const source = readFileSync(join(BRIDGE_DIR, 'bridge.js'), 'utf8');
        // Static custom properties are set by literal name.
        for (const variable of [
            '--raven-bg',
            '--raven-code-bg',
            '--raven-fg',
            '--raven-font-text',
            '--raven-font-mono',
        ]) {
            expect(source).toContain(variable);
        }
        // Per-role custom properties are derived from the role key, whose set is
        // pinned to the shared protocol by the first test in this suite.
        expect(source).toContain("'--raven-c-' + role");
    });

    test('font validator accepts sanitizer-emitted C0 characters', () => {
        const source = readFileSync(join(BRIDGE_DIR, 'bridge.js'), 'utf8');
        const literal = source.match(/var fontPattern = (\/.*\/);/)?.[1];
        expect(literal).toBeDefined();
        const pattern = new Function(`return ${literal}`)() as RegExp;
        expect(pattern.test('Control\u0001Font, sans-serif')).toBe(true);
        expect(pattern.test('unsafe;font')).toBe(false);
    });

    test('scopes surface overrides and consumes every theme variable', () => {
        const css = readFileSync(join(BRIDGE_DIR, 'bridge.css'), 'utf8');
        expect(css).toContain('html.raven-vscode-theme');
        for (const variable of [
            '--raven-bg', '--raven-code-bg', '--raven-fg',
            '--raven-c-keyword', '--raven-c-string',
            '--raven-c-number', '--raven-c-comment', '--raven-c-function',
            '--raven-c-type', '--raven-c-variable', '--raven-c-operator',
            '--raven-c-punctuation', '--raven-c-constant', '--raven-font-text',
            '--raven-font-mono',
        ]) {
            expect(css).toContain(`var(${variable})`);
        }
        for (const surface of ['.navbar', '#quarto-sidebar', '#TOC', '.callout', '.card', 'table']) {
            expect(css).toContain(surface);
        }
    });

    test('overrides Quarto normal syntax color on Pandoc line wrappers', () => {
        const css = readFileSync(join(BRIDGE_DIR, 'bridge.css'), 'utf8');
        expect(css).toMatch(
            /html\.raven-vscode-theme code\.sourceCode > span\s*\{[^}]*color:\s*var\(--raven-fg\)\s*!important;/s,
        );
    });

    test('paints code surfaces once with the VS Code code-block background', () => {
        const css = readFileSync(join(BRIDGE_DIR, 'bridge.css'), 'utf8');
        expect(css).toMatch(
            /html\.raven-vscode-theme pre\s*\{[^}]*background-color:\s*var\(--raven-code-bg\)\s*!important;/s,
        );
        expect(css).toMatch(
            /html\.raven-vscode-theme pre code\s*\{[^}]*background-color:\s*transparent\s*!important;/s,
        );
        expect(css).toMatch(
            /html\.raven-vscode-theme \.cell-output pre\s*\{[^}]*background-color:\s*var\(--raven-bg\)\s*!important;/s,
        );
    });
});

function readArrayLiteral(source: string, name: string): unknown[] {
    const literal = source.match(new RegExp(`var ${name} = (\\[[\\s\\S]*?\\]);`))?.[1];
    if (!literal) throw new Error(`missing ${name} array literal`);
    return new Function(`return ${literal}`)() as unknown[];
}

function readRegexLiteral(source: string, name: string): RegExp {
    const literal = source.match(new RegExp(`var ${name} = (/.*?/[i]*);`))?.[1];
    if (!literal) throw new Error(`missing ${name} regex literal`);
    return new Function(`return ${literal}`)() as RegExp;
}
