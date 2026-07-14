/**
 * Pure outer-shell builder for the Quarto preview webview.
 *
 * Workspace-controlled Quarto output runs only in a genuinely cross-origin,
 * sandboxed iframe. The outer shell never reads the framed DOM and has no
 * network permissions of its own. Its message listener rejects the embedded
 * frame as a sender, accepts only the empty/webview-host origin used by VS
 * Code delivery, and mirrors the host protocol's exact-key validation before
 * rendering state. Serving CSP is derived from the same mapped URL installed
 * in the frame; non-serving states have neither a `frame-src` directive nor
 * an iframe.
 *
 * All dynamic state enters the script through JSON serialized with `<`
 * escaped, then reaches visible DOM through `textContent`. The serving URL
 * is additionally HTML-attribute escaped for `src`. No CLI output, path, or
 * URL is interpolated as executable markup.
 */

import type { QuartoPreviewViewState } from './quarto-messages';

export interface QuartoPreviewShellHtmlArgs {
    nonce: string;
    sourceFsPath: string;
    state: QuartoPreviewViewState;
}

function escapeHtmlAttribute(value: string): string {
    return value
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
}

function jsonForScript(value: unknown): string {
    return JSON.stringify(value).replace(/</g, '\\u003c');
}

function servingOrigin(state: QuartoPreviewViewState): string | null {
    if (state.kind !== 'serving') return null;
    try {
        const parsed = new URL(state.externalUrl);
        if (parsed.origin === 'null') return null;
        return parsed.origin;
    } catch {
        return null;
    }
}

/**
 * Build a self-contained Raven-controlled Quarto preview shell.
 *
 * The caller must pass the fresh `asExternalUri` result in serving state's
 * `externalUrl`. This function derives both CSP `frame-src` and iframe `src`
 * from that same value in one render, preventing mapping/CSP drift.
 */
export function buildQuartoPreviewShellHtml(args: QuartoPreviewShellHtmlArgs): string {
    const { nonce, sourceFsPath, state } = args;
    const origin = servingOrigin(state);
    const isServing = state.kind === 'serving' && origin !== null;
    const csp = [
        `default-src 'none'`,
        ...(isServing ? [`frame-src ${origin}`] : []),
        `script-src 'nonce-${nonce}'`,
        `style-src 'nonce-${nonce}'`,
    ].join('; ');
    const seed = jsonForScript({ sourceFsPath, state });
    const safeNonce = escapeHtmlAttribute(nonce);
    const frame = isServing
        ? `<iframe id="raven-quarto-frame"
          src="${escapeHtmlAttribute(state.externalUrl)}"
          sandbox="allow-scripts allow-same-origin allow-forms allow-downloads"
          title="Quarto preview"></iframe>`
        : '';

    return `<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="${escapeHtmlAttribute(csp)}">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Quarto Preview</title>
    <style nonce="${safeNonce}">
        html, body { height: 100%; }
        body {
            margin: 0;
            display: flex;
            flex-direction: column;
            color: var(--vscode-foreground);
            background: var(--vscode-editor-background);
            font-family: var(--vscode-font-family);
        }
        #raven-quarto-toolbar {
            display: flex;
            align-items: center;
            gap: 6px;
            padding: 6px 8px;
            flex: 0 0 auto;
            border-bottom: 1px solid var(--vscode-panel-border);
            background: var(--vscode-editorWidget-background);
        }
        button {
            border: 0;
            border-radius: 2px;
            padding: 4px 8px;
            color: var(--vscode-button-secondaryForeground);
            background: var(--vscode-button-secondaryBackground);
            font: inherit;
            cursor: pointer;
        }
        button:hover { background: var(--vscode-button-secondaryHoverBackground); }
        #raven-quarto-status { padding: 16px; white-space: pre-wrap; }
        #raven-quarto-frame { flex: 1 1 auto; width: 100%; border: 0; background: white; }
        #raven-quarto-load-banner {
            padding: 8px 12px;
            border-bottom: 1px solid var(--vscode-panel-border);
            background: var(--vscode-inputValidation-warningBackground);
            color: var(--vscode-inputValidation-warningForeground);
        }
        [hidden] { display: none !important; }
    </style>
</head>
<body>
    <div id="raven-quarto-toolbar" role="toolbar" aria-label="Quarto preview">
        <button id="raven-quarto-open" type="button">Open in Browser</button>
        <button id="raven-quarto-stop" type="button">Stop Preview</button>
        <button id="raven-quarto-restart" type="button">Restart Preview</button>
    </div>
    <div id="raven-quarto-load-banner" role="status" hidden></div>
    <div id="raven-quarto-status" role="status"></div>
    ${frame}
    <script nonce="${safeNonce}">
        (function () {
            const vscode = acquireVsCodeApi();
            const initial = ${seed};
            try { vscode.setState({ sourceFsPath: initial.sourceFsPath }); } catch (_) {}

            const status = document.getElementById('raven-quarto-status');
            const openButton = document.getElementById('raven-quarto-open');
            const stopButton = document.getElementById('raven-quarto-stop');
            const restartButton = document.getElementById('raven-quarto-restart');
            const banner = document.getElementById('raven-quarto-load-banner');
            const frame = document.getElementById('raven-quarto-frame');

            function isRecord(value) {
                return value !== null && typeof value === 'object' && !Array.isArray(value);
            }

            function hasExactKeys(value, expected) {
                const actual = Object.keys(value).sort();
                if (actual.length !== expected.length) return false;
                for (let i = 0; i < expected.length; i++) {
                    if (actual[i] !== expected[i]) return false;
                }
                return true;
            }

            function isViewState(value) {
                if (!isRecord(value) || typeof value.kind !== 'string') return false;
                const schemas = {
                    'starting': ['kind'],
                    'serving': ['externalUrl', 'kind'],
                    'failed': ['detailText', 'kind'],
                    'exited-unexpectedly': ['code', 'kind'],
                    'stopped': ['kind'],
                    'restore-placeholder': ['kind']
                };
                if (!Object.prototype.hasOwnProperty.call(schemas, value.kind)) return false;
                if (!hasExactKeys(value, schemas[value.kind])) return false;
                switch (value.kind) {
                    case 'serving': return typeof value.externalUrl === 'string';
                    case 'failed': return typeof value.detailText === 'string';
                    case 'exited-unexpectedly':
                        return value.code === null
                            || (typeof value.code === 'number' && Number.isInteger(value.code));
                    default: return true;
                }
            }

            function isStateUpdate(value) {
                return isRecord(value)
                    && hasExactKeys(value, ['payload', 'type'])
                    && value.type === 'state-update'
                    && isViewState(value.payload);
            }

            function render(next) {
                status.textContent = '';
                status.hidden = next.kind === 'serving';
                openButton.hidden = next.kind !== 'serving';
                stopButton.hidden = next.kind !== 'starting' && next.kind !== 'serving';
                restartButton.hidden = next.kind === 'starting' || next.kind === 'serving';

                switch (next.kind) {
                    case 'starting':
                        status.textContent = 'Starting Quarto preview…';
                        break;
                    case 'serving':
                        break;
                    case 'failed':
                        status.textContent = next.detailText;
                        break;
                    case 'exited-unexpectedly':
                        status.textContent = next.code === null
                            ? 'Quarto preview exited unexpectedly.'
                            : 'Quarto preview exited unexpectedly (code ' + String(next.code) + ').';
                        break;
                    case 'stopped':
                        status.textContent = 'Quarto preview stopped.';
                        break;
                    case 'restore-placeholder':
                        status.textContent = 'This restored preview is not running. Restart it to continue.';
                        break;
                }
            }

            openButton.addEventListener('click', function () {
                vscode.postMessage({ type: 'open-in-browser' });
            });
            stopButton.addEventListener('click', function () {
                vscode.postMessage({ type: 'stop-preview' });
            });
            restartButton.addEventListener('click', function () {
                vscode.postMessage({ type: 'request-restart' });
            });
            window.addEventListener('message', function (event) {
                if (event.origin !== '' && event.origin !== window.origin) return;
                if (frame && event.source === frame.contentWindow) return;
                const message = event.data;
                if (!isStateUpdate(message)) return;
                render(message.payload);
            });

            render(initial.state);
            if (frame) {
                window.setTimeout(function () {
                    banner.textContent = 'If the preview looks blank, try Open in Browser.';
                    banner.hidden = false;
                    vscode.postMessage({ type: 'load-timeout' });
                }, 8000);
            }
            vscode.postMessage({ type: 'webview-ready' });
        }());
    </script>
</body>
</html>`;
}
