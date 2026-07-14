import { describe, expect, test } from 'bun:test';
import {
    isExtensionToPreviewMessage,
    isPreviewToExtensionMessage,
} from '../../editors/vscode/src/quarto/quarto-messages';

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
    });
});
