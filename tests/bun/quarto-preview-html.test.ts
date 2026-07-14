import { describe, expect, test } from 'bun:test';
import {
    buildQuartoPreviewShellHtml,
    type QuartoPreviewShellHtmlArgs,
} from '../../editors/vscode/src/quarto/quarto-preview-html';
import type { QuartoPreviewViewState } from '../../editors/vscode/src/quarto/quarto-messages';

function args(state: QuartoPreviewViewState): QuartoPreviewShellHtmlArgs {
    return {
        nonce: 'NONCE123',
        sourceFsPath: '/work/report.qmd',
        state,
    };
}

describe('buildQuartoPreviewShellHtml', () => {
    test('serving state pins frame-src to the mapped origin', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'https://mapped.example.test/preview/?x=1' }),
        );
        expect(html).toContain('frame-src https://mapped.example.test');
        expect(html).not.toContain('frame-src https://mapped.example.test/preview');
        expect(html).toContain('src="https://mapped.example.test/preview/?x=1"');
        expect(html).toContain("default-src 'none'");
        expect(html).toContain("script-src 'nonce-NONCE123'");
        expect(html).toContain("style-src 'nonce-NONCE123'");
    });

    test('uses the exact required iframe sandbox', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        expect(html).toMatch(
            /<iframe[^>]*sandbox="allow-scripts allow-same-origin allow-forms allow-downloads"/,
        );
        expect(html).not.toContain('allow-popups');
        expect(html).not.toContain('allow-top-navigation');
    });

    test('non-serving states have no frame-src and no iframe', () => {
        const states: QuartoPreviewViewState[] = [
            { kind: 'starting' },
            { kind: 'failed', detailText: 'failed' },
            { kind: 'exited-unexpectedly', code: 2 },
            { kind: 'stopped' },
            { kind: 'restore-placeholder' },
        ];
        for (const state of states) {
            const html = buildQuartoPreviewShellHtml(args(state));
            expect(html).not.toContain('frame-src');
            expect(html).not.toContain('<iframe');
        }
    });

    test('routes toolbar actions through outer-shell messages only', () => {
        const html = buildQuartoPreviewShellHtml(args({ kind: 'starting' }));
        expect(html).toContain("vscode.postMessage({ type: 'open-in-browser' })");
        expect(html).toContain("vscode.postMessage({ type: 'stop-preview' })");
        expect(html).toContain("vscode.postMessage({ type: 'request-restart' })");
    });

    test('accepts only host-origin exact-shape state updates, never iframe messages', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        expect(html).toContain("event.origin !== '' && event.origin !== window.origin");
        expect(html).toContain('event.source === frame.contentWindow');
        expect(html).toContain("hasExactKeys(value, ['payload', 'type'])");
        expect(html).toContain("'serving': ['externalUrl', 'kind']");
        expect(html).toContain("'failed': ['detailText', 'kind']");
        expect(html).toContain("'exited-unexpectedly': ['code', 'kind']");
        expect(html).toContain('if (!isStateUpdate(message)) return');
    });

    test('persists only sourceFsPath and installs the honest timeout banner', () => {
        const html = buildQuartoPreviewShellHtml(args({ kind: 'serving', externalUrl: 'http://localhost:9/' }));
        expect(html).toContain('vscode.setState({ sourceFsPath: initial.sourceFsPath })');
        expect(html).toContain('If the preview looks blank, try Open in Browser.');
        expect(html).toContain("vscode.postMessage({ type: 'load-timeout' })");
        expect(html).toContain('}, 8000)');
    });

    test('never injects hostile detail text, paths, or URLs as raw markup/script', () => {
        const hostile = '</script>"<angle>&';
        const failedHtml = buildQuartoPreviewShellHtml({
            nonce: 'NONCE123',
            sourceFsPath: `/work/${hostile}.qmd`,
            state: { kind: 'failed', detailText: `failure: ${hostile}` },
        });
        expect(failedHtml).not.toContain(hostile);
        expect(failedHtml).toContain('\\u003c/script>');
        expect(failedHtml).toContain('status.textContent = next.detailText');

        const hostileUrl = `https://mapped.example.test/${hostile}`;
        const servingHtml = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: hostileUrl }),
        );
        expect(servingHtml).not.toContain(hostileUrl);
        expect(servingHtml).toContain('&lt;/script&gt;&quot;&lt;angle&gt;&amp;');
    });
});
