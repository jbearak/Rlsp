import { describe, expect, test } from 'bun:test';
import {
    isExtensionToPreviewMessage,
    isPreviewToExtensionMessage,
    isQuartoThemePayload,
    isRavenQuartoThemeMessage,
    isRavenQuartoThemePingMessage,
    isRavenQuartoThemeReadyMessage,
    type QuartoThemePayload,
} from '../../editors/vscode/src/quarto/quarto-messages';
import { sanitizeFontFamily } from '../../editors/vscode/src/knit/render-html';

function themePayload(): QuartoThemePayload {
    return {
        enabled: true,
        background: '#1e1e1e',
        foreground: 'rgb(212, 212, 212)',
        roles: {
            keyword: '#c586c0',
            string: '#ce9178',
            number: '#b5cea8',
            comment: '#6a9955',
            function: '#dcdcaa',
            type: '#4ec9b0',
            variable: '#9cdcfe',
            operator: '#d4d4d4',
            punctuation: '#808080',
            constant: 'rgba(86, 156, 214, 0.9)',
        },
        fontText: '-apple-system, "Segoe UI", sans-serif',
        fontMono: '"SFMono-Regular", Consolas, monospace',
    };
}

describe('Quarto preview message validators', () => {
    test('accepts every extension-to-preview view state', () => {
        const states = [
            { kind: 'starting' },
            { kind: 'serving', externalUrl: 'https://tunnel.example/preview/' },
            { kind: 'failed', detailText: 'Quarto failed' },
            { kind: 'exited-unexpectedly', code: 1 },
            { kind: 'exited-unexpectedly', code: null },
            { kind: 'stopped' },
            { kind: 'restore-placeholder' },
        ];
        for (const payload of states) {
            expect(isExtensionToPreviewMessage({ type: 'state-update', payload })).toBe(true);
        }
    });

    test('rejects missing and extra extension-message keys at both levels', () => {
        expect(isExtensionToPreviewMessage({ type: 'state-update' })).toBe(false);
        expect(
            isExtensionToPreviewMessage({
                type: 'state-update',
                payload: { kind: 'starting', injected: true },
            }),
        ).toBe(false);
        expect(
            isExtensionToPreviewMessage({
                type: 'state-update',
                payload: { kind: 'serving' },
            }),
        ).toBe(false);
        expect(
            isExtensionToPreviewMessage({
                type: 'state-update',
                payload: { kind: 'stopped' },
                extra: true,
            }),
        ).toBe(false);
    });

    test('accepts new extension theme messages and rejects malformed variants', () => {
        expect(isExtensionToPreviewMessage({
            type: 'theme-update',
            payload: themePayload(),
        })).toBe(true);
        expect(isExtensionToPreviewMessage({ type: 'theme-context-request' })).toBe(true);
        expect(isExtensionToPreviewMessage({
            type: 'theme-context-request',
            extra: true,
        })).toBe(false);
        expect(isExtensionToPreviewMessage({
            type: 'theme-update',
            payload: { ...themePayload(), enabled: 'yes' },
        })).toBe(false);
    });

    test('accepts every preview-to-extension action', () => {
        for (const type of [
            'webview-ready',
            'open-in-browser',
            'stop-preview',
            'request-restart',
            'load-timeout',
        ]) {
            expect(isPreviewToExtensionMessage({ type })).toBe(true);
        }
        expect(
            isPreviewToExtensionMessage({ type: 'report-error', message: 'frame failed' }),
        ).toBe(true);
        expect(isPreviewToExtensionMessage({
            type: 'theme-context',
            background: '#1e1e1e',
        })).toBe(true);
        expect(isPreviewToExtensionMessage({ type: 'theme-changed', applied: true })).toBe(true);
        expect(isPreviewToExtensionMessage({
            type: 'theme-bridge-status',
            available: false,
        })).toBe(true);
    });

    test('rejects missing, extra, malformed, and unknown preview messages', () => {
        expect(isPreviewToExtensionMessage({ type: 'open-in-browser', url: 'https://evil/' })).toBe(false);
        expect(isPreviewToExtensionMessage({ type: 'report-error' })).toBe(false);
        expect(isPreviewToExtensionMessage({ type: 'report-error', message: 12 })).toBe(false);
        expect(
            isPreviewToExtensionMessage({ type: 'report-error', message: 'x', extra: true }),
        ).toBe(false);
        expect(isPreviewToExtensionMessage({ type: 'unknown' })).toBe(false);
        expect(isPreviewToExtensionMessage(null)).toBe(false);
        expect(isPreviewToExtensionMessage({
            type: 'theme-context',
            background: 12,
        })).toBe(false);
        expect(isPreviewToExtensionMessage({
            type: 'theme-changed',
            applied: 'true',
        })).toBe(false);
    });
});

describe('Quarto theme payload validators', () => {
    test('accepts the exact safe payload and bridge message schemas', () => {
        const payload = themePayload();
        expect(isQuartoThemePayload(payload)).toBe(true);
        expect(isRavenQuartoThemeMessage({
            __ravenQuartoTheme: true,
            ...payload,
        })).toBe(true);
        expect(isRavenQuartoThemeReadyMessage({
            type: 'raven-quarto-theme-ready',
        })).toBe(true);
        expect(isRavenQuartoThemePingMessage({
            type: 'raven-quarto-theme-ping',
        })).toBe(true);
    });

    test('rejects missing and extra payload keys', () => {
        const { fontMono: _missing, ...missing } = themePayload();
        expect(isQuartoThemePayload(missing)).toBe(false);
        expect(isQuartoThemePayload({ ...themePayload(), extra: true })).toBe(false);
    });

    test('rejects unsafe colors and fonts', () => {
        expect(isQuartoThemePayload({
            ...themePayload(),
            background: 'red; background:url(evil)',
        })).toBe(false);
        expect(isQuartoThemePayload({
            ...themePayload(),
            fontText: 'system-ui; color: red',
        })).toBe(false);
        expect(isQuartoThemePayload({
            ...themePayload(),
            fontMono: 'mono\\evil',
        })).toBe(false);
    });

    test('accepts every character shape that sanitizeFontFamily can emit', () => {
        const font = 'Control\u0001Font, sans-serif';
        expect(sanitizeFontFamily(font)).toBe(font);
        expect(isQuartoThemePayload({
            ...themePayload(),
            fontText: font,
            fontMono: font,
        })).toBe(true);
    });

    test('rejects wrong, missing, extra, and malformed role sets', () => {
        const payload = themePayload();
        const { constant: _missing, ...missingRole } = payload.roles;
        expect(isQuartoThemePayload({ ...payload, roles: missingRole })).toBe(false);
        expect(isQuartoThemePayload({
            ...payload,
            roles: { ...payload.roles, builtin: '#ffffff' },
        })).toBe(false);
        expect(isQuartoThemePayload({
            ...payload,
            roles: { ...payload.roles, keyword: 'hsl(10, 10%, 10%)' },
        })).toBe(false);
    });

    test('rejects bridge marker and handshake schema smuggling', () => {
        expect(isRavenQuartoThemeMessage({
            __ravenQuartoTheme: false,
            ...themePayload(),
        })).toBe(false);
        expect(isRavenQuartoThemeReadyMessage({
            type: 'raven-quarto-theme-ready',
            payload: {},
        })).toBe(false);
        expect(isRavenQuartoThemePingMessage({
            type: 'raven-quarto-theme-ping',
            extra: true,
        })).toBe(false);
    });
});
