import { describe, expect, test } from 'bun:test';
import { JSDOM } from 'jsdom';
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
        themeEnabled: false,
    };
}

describe('buildQuartoPreviewShellHtml', () => {
    test('resets host body spacing so the shell fills the webview', () => {
        const html = buildQuartoPreviewShellHtml(args({ kind: 'starting' }));
        expect(html).toMatch(/body\s*\{[^}]*margin:\s*0;[^}]*padding:\s*0;/);
    });

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

    test('keeps frame and host delivery in disjoint security branches', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        const frameBranch = html.indexOf('if (frame && event.source === frame.contentWindow)');
        const frameReturn = html.indexOf('return;', frameBranch);
        const hostBranch = html.indexOf(
            "if (event.origin === '' || event.origin === window.origin)",
        );
        expect(frameBranch).toBeGreaterThan(0);
        expect(frameReturn).toBeGreaterThan(frameBranch);
        expect(hostBranch).toBeGreaterThan(frameReturn);
        expect(html).toContain('event.origin === frameOrigin && isThemeReady(message)');
        expect(html).toContain("hasExactKeys(value, ['payload', 'type'])");
        expect(html).toContain("'serving': ['externalUrl', 'kind']");
        expect(html).toContain("'failed': ['detailText', 'kind']");
        expect(html).toContain("'exited-unexpectedly': ['code', 'kind']");
        expect(html).toContain('if (isStateUpdate(message))');
        // A forged frame-origin state update returns in Branch A; a forged
        // non-frame theme-context message misses Branch B's host origins.
        expect(html).toContain('value.type === \'raven-quarto-theme-ready\'');
        expect(html).toContain("value.type === 'theme-context-request'");
    });

    test('caches host themes but a ready handshake does not resend', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        expect(html).toContain('currentTheme = message.payload');
        expect(html).toContain('__ravenQuartoTheme: true');
        expect(html).toContain('}, frameOrigin)');
        const readyStart = html.indexOf(
            'if (event.origin === frameOrigin && isThemeReady(message))',
        );
        const readyEnd = html.indexOf('\n                    return;', readyStart);
        const readyBranch = html.slice(readyStart, readyEnd);
        expect(readyBranch).toContain('markBridgeAvailable();');
        expect(readyBranch).not.toContain('postThemeToFrame();');
        expect(html).toContain("hasExactKeys(value, ['type'])");
    });

    test('emits shared color/font validators with live regex escapes', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        const colorLiteral = html.match(
            /const colorPattern = new RegExp\(("(?:[^"\\]|\\.)*")?, 'i'\)/,
        )?.[1];
        const fontLiteral = html.match(
            /const fontPattern = new RegExp\(("(?:[^"\\]|\\.)*")?\)/,
        )?.[1];
        expect(colorLiteral).toBeDefined();
        expect(fontLiteral).toBeDefined();
        const colorPattern = new RegExp(JSON.parse(colorLiteral!), 'i');
        const fontPattern = new RegExp(JSON.parse(fontLiteral!));
        expect(colorPattern.test('#a1b2c3')).toBe(true);
        expect(colorPattern.test('rgb(1, 22, 255)')).toBe(true);
        expect(colorPattern.test('rgba(1, 22, 255, 0.5)')).toBe(true);
        expect(fontPattern.test('Control\u0001Font, sans-serif')).toBe(true);
    });

    test('contains the persisted toggle and responsive host relay', () => {
        const html = buildQuartoPreviewShellHtml({
            ...args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
            themeEnabled: true,
        });
        expect(html).toMatch(
            /<button[^>]*id="raven-quarto-theme"[^>]*[\s\S]*?aria-pressed="true"/,
        );
        expect(html).toContain('themeEnabled = !themeEnabled');
        expect(html).toContain('currentTheme = { ...currentTheme, enabled: themeEnabled }');
        expect(html).toContain("vscode.postMessage({ type: 'theme-changed', applied: themeEnabled })");
        expect(html).toContain('themeEnabled = currentTheme.enabled');
    });

    test('persists only sourceFsPath and installs a recoverable timeout banner', () => {
        const html = buildQuartoPreviewShellHtml(args({ kind: 'serving', externalUrl: 'http://localhost:9/' }));
        expect(html).toContain('vscode.setState({ sourceFsPath: initial.sourceFsPath })');
        expect(html).toContain('If the preview looks blank, try Open in Browser.');
        expect(html).toContain("vscode.postMessage({ type: 'load-timeout' })");
        expect(html).toContain('clearBlankAdvisory();');
        expect(html).toContain("banner.textContent = ''");
        expect(html).toContain('banner.hidden = true');
        expect(html).toContain('}, 8000)');
    });

    test('re-sends on load and times out an unavailable bridge', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://localhost:9/' }),
        );
        expect(html).toContain("frame.addEventListener('load', function ()");
        expect(html).toContain('postThemeToFrame();');
        expect(html).toContain('armBridgeTimeout();');
        expect(html).toContain('postBridgePing();');
        expect(html).toContain("type: 'raven-quarto-theme-ping'");
        expect(html).toContain('window.clearTimeout(blankTimer)');
        expect(html).toContain("vscode.postMessage({ type: 'theme-bridge-status', available: false })");
        expect(html).toContain("VS Code theme can't apply to this page");
        expect(html).not.toContain('themeButton.disabled = bridgeAvailable === false');
        expect(html).not.toContain('if (themeButton.disabled) return');
        expect(html).toContain('}, 3000)');
    });

    test('never injects hostile detail text, paths, or URLs as raw markup/script', () => {
        const hostile = '</script>"<angle>&';
        const failedHtml = buildQuartoPreviewShellHtml({
            nonce: 'NONCE123',
            sourceFsPath: `/work/${hostile}.qmd`,
            state: { kind: 'failed', detailText: `failure: ${hostile}` },
            themeEnabled: false,
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

    test('emits syntactically valid shell JavaScript', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        const match = html.match(/<script nonce="NONCE123">([\s\S]*?)<\/script>/);
        expect(match).not.toBeNull();
        expect(() => new Function(match![1])).not.toThrow();
    });

    test('enforces frame/host branches and resends cached themes at runtime', () => {
        const hostMessages: unknown[] = [];
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        const dom = new JSDOM(html, { url: 'https://webview.example.test/' });
        try {
            const { window } = dom;
            const frame = window.document.getElementById(
                'raven-quarto-frame',
            ) as HTMLIFrameElement;
            const status = window.document.getElementById('raven-quarto-status')!;
            const frameMessages: unknown[] = [];
            frame.contentWindow!.postMessage = (message: unknown) => {
                frameMessages.push(message);
            };
            const script = html.match(/<script nonce="NONCE123">([\s\S]*?)<\/script>/)![1];
            const run = new Function(
                'window',
                'document',
                'acquireVsCodeApi',
                'getComputedStyle',
                script,
            );
            run(
                window,
                window.document,
                () => ({
                    setState: () => undefined,
                    postMessage: (message: unknown) => { hostMessages.push(message); },
                }),
                window.getComputedStyle.bind(window),
            );

            window.dispatchEvent(new window.MessageEvent('message', {
                origin: 'http://127.0.0.1:4000',
                source: frame.contentWindow,
                data: {
                    type: 'state-update',
                    payload: { kind: 'stopped' },
                },
            }));
            expect(status.textContent).toBe('');

            const contextsBefore = hostMessages.filter((message) =>
                (message as { type?: unknown })?.type === 'theme-context'
            ).length;
            window.dispatchEvent(new window.MessageEvent('message', {
                origin: 'https://evil.example.test',
                data: { type: 'theme-context-request' },
            }));
            expect(hostMessages.filter((message) =>
                (message as { type?: unknown })?.type === 'theme-context'
            )).toHaveLength(contextsBefore);

            window.dispatchEvent(new window.MessageEvent('message', {
                origin: '',
                data: { type: 'theme-update', payload: themePayload() },
            }));
            expect(frameMessages).toHaveLength(1);
            expect(frameMessages[0]).toMatchObject({
                __ravenQuartoTheme: true,
                enabled: true,
            });

            window.dispatchEvent(new window.MessageEvent('message', {
                origin: 'http://127.0.0.1:4000',
                source: frame.contentWindow,
                data: { type: 'raven-quarto-theme-ready' },
            }));
            expect(frameMessages.filter((message) =>
                (message as { __ravenQuartoTheme?: unknown }).__ravenQuartoTheme === true
            )).toHaveLength(1);
            expect(hostMessages).toContainEqual({
                type: 'theme-bridge-status',
                available: true,
            });
        } finally {
            dom.window.close();
        }
    });

    test('re-arms availability on every navigation and a later ready re-enables', () => {
        const html = buildQuartoPreviewShellHtml(
            args({ kind: 'serving', externalUrl: 'http://127.0.0.1:4000/' }),
        );
        const dom = new JSDOM(html, { url: 'https://webview.example.test/' });
        try {
            const { window } = dom;
            const hostMessages: unknown[] = [];
            const frameMessages: unknown[] = [];
            const timers: Array<{ callback: () => void; delay: number; active: boolean }> = [];
            window.setTimeout = ((callback: () => void, delay: number) => {
                timers.push({ callback, delay, active: true });
                return timers.length;
            }) as typeof window.setTimeout;
            window.clearTimeout = ((id: number) => {
                if (timers[id - 1]) timers[id - 1].active = false;
            }) as typeof window.clearTimeout;
            const script = html.match(/<script nonce="NONCE123">([\s\S]*?)<\/script>/)![1];
            const run = new Function(
                'window',
                'document',
                'acquireVsCodeApi',
                'getComputedStyle',
                script,
            );
            run(
                window,
                window.document,
                () => ({
                    setState: () => undefined,
                    postMessage: (message: unknown) => { hostMessages.push(message); },
                }),
                window.getComputedStyle.bind(window),
            );
            const frame = window.document.getElementById(
                'raven-quarto-frame',
            ) as HTMLIFrameElement;
            frame.contentWindow!.postMessage = (message: unknown) => {
                frameMessages.push(message);
            };
            const themeButton = window.document.getElementById(
                'raven-quarto-theme',
            ) as HTMLButtonElement;
            const banner = window.document.getElementById('raven-quarto-load-banner')!;

            // The advisory may win a slow-load race, but a later trusted
            // bridge handshake must dismiss it and keep it dismissed.
            const blank = timers.find((timer) => timer.delay === 8000);
            expect(blank).toBeDefined();
            blank!.callback();
            blank!.active = false;
            expect(banner.hidden).toBe(false);
            expect(banner.textContent).toContain('try Open in Browser');

            frame.dispatchEvent(new window.Event('load'));
            expect(frameMessages.at(-1)).toEqual({ type: 'raven-quarto-theme-ping' });
            window.dispatchEvent(new window.MessageEvent('message', {
                origin: 'http://127.0.0.1:4000',
                source: frame.contentWindow,
                data: { type: 'raven-quarto-theme-ready' },
            }));
            expect(banner.hidden).toBe(true);
            expect(banner.textContent).toBe('');

            // A subsequent CSP-blocked/non-HTML navigation never answers its
            // load-triggered ping, but the global preference remains editable.
            frame.dispatchEvent(new window.Event('load'));
            const blockedTimeout = [...timers].reverse().find((timer) =>
                timer.delay === 3000 && timer.active
            );
            expect(blockedTimeout).toBeDefined();
            blockedTimeout!.callback();
            blockedTimeout!.active = false;
            expect(themeButton.disabled).toBe(false);
            expect(themeButton.title).toBe("VS Code theme can't apply to this page");
            expect(hostMessages).toContainEqual({
                type: 'theme-bridge-status',
                available: false,
            });
            themeButton.click();
            expect(themeButton.getAttribute('aria-pressed')).toBe('true');
            expect(hostMessages).toContainEqual({
                type: 'theme-changed',
                applied: true,
            });

            // A normal later page responds to its own ping and re-enables.
            frame.dispatchEvent(new window.Event('load'));
            window.dispatchEvent(new window.MessageEvent('message', {
                origin: 'http://127.0.0.1:4000',
                source: frame.contentWindow,
                data: { type: 'raven-quarto-theme-ready' },
            }));
            expect(themeButton.disabled).toBe(false);
            expect(themeButton.title).toBe('Apply VS Code theme');
            expect(hostMessages.filter((message) =>
                (message as { type?: unknown }).type === 'theme-bridge-status'
            )).toEqual([
                { type: 'theme-bridge-status', available: true },
                { type: 'theme-bridge-status', available: false },
                { type: 'theme-bridge-status', available: true },
            ]);
            expect(frameMessages.filter((message) =>
                (message as { type?: unknown }).type === 'raven-quarto-theme-ping'
            )).toHaveLength(3);

            expect(banner.hidden).toBe(true);
        } finally {
            dom.window.close();
        }
    });
});

function themePayload() {
    return {
        enabled: true,
        background: '#1e1e1e',
        foreground: '#d4d4d4',
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
            constant: '#569cd6',
        },
        fontText: 'Inter, sans-serif',
        fontMono: 'Mono, monospace',
    };
}
