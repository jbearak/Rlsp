/**
 * Pure outer-shell builder for the Quarto preview webview.
 *
 * Workspace-controlled Quarto output runs only in a genuinely cross-origin,
 * sandboxed iframe. The outer shell never reads the framed DOM and has no
 * network permissions of its own. Its message listener gives the embedded
 * frame one exact, origin-pinned ready-handshake shape and returns for every
 * other frame message. A disjoint branch accepts host delivery only from the
 * empty/webview-host origin and mirrors the host protocol's exact-key guards.
 * Because iframe `load` is not proof of success, only that ready handshake
 * cancels or dismisses the conservative blank-preview advisory.
 * Serving CSP is derived from the same mapped URL installed in the frame;
 * non-serving states have neither a `frame-src` directive nor an iframe.
 *
 * All dynamic state enters the script through JSON serialized with `<`
 * escaped, then reaches visible DOM through `textContent`. The serving URL
 * is additionally HTML-attribute escaped for `src`. No CLI output, path, or
 * URL is interpolated as executable markup.
 */

import {
    QUARTO_CSS_COLOR_PATTERN_SOURCE,
    QUARTO_SANITIZED_FONT_PATTERN_SOURCE,
    type QuartoPreviewViewState,
} from './quarto-messages';

export interface QuartoPreviewShellHtmlArgs {
    nonce: string;
    sourceFsPath: string;
    state: QuartoPreviewViewState;
    themeEnabled: boolean;
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
    const { nonce, sourceFsPath, state, themeEnabled } = args;
    const origin = servingOrigin(state);
    const isServing = state.kind === 'serving' && origin !== null;
    const csp = [
        `default-src 'none'`,
        ...(isServing ? [`frame-src ${origin}`] : []),
        `script-src 'nonce-${nonce}'`,
        `style-src 'nonce-${nonce}'`,
    ].join('; ');
    const seed = jsonForScript({
        sourceFsPath,
        state,
        themeEnabled,
        frameOrigin: origin,
    });
    const colorPatternSource = jsonForScript(QUARTO_CSS_COLOR_PATTERN_SOURCE);
    const fontPatternSource = jsonForScript(QUARTO_SANITIZED_FONT_PATTERN_SOURCE);
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
        button[aria-pressed="true"] {
            color: var(--vscode-button-foreground);
            background: var(--vscode-button-background);
        }
        button[aria-pressed="true"]:hover {
            background: var(--vscode-button-hoverBackground);
        }
        button:disabled { cursor: default; opacity: 0.55; }
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
        <button id="raven-quarto-theme" type="button"
          aria-pressed="${themeEnabled ? 'true' : 'false'}">Apply VS Code theme</button>
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
            const themeButton = document.getElementById('raven-quarto-theme');
            const banner = document.getElementById('raven-quarto-load-banner');
            const frame = document.getElementById('raven-quarto-frame');
            const frameOrigin = initial.frameOrigin;
            let currentState = initial.state;
            let themeEnabled = initial.themeEnabled === true;
            let currentTheme = null;
            let blankTimer = null;
            let bridgeTimer = null;
            let bridgeAvailable = null;

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

            const roleKeys = [
                'keyword', 'string', 'number', 'comment', 'function',
                'type', 'variable', 'operator', 'punctuation', 'constant'
            ];
            const payloadKeys = [
                'background', 'enabled', 'fontMono', 'fontText',
                'foreground', 'roles'
            ];
            const roleSchemaKeys = roleKeys.slice().sort();
            const colorPattern = new RegExp(${colorPatternSource}, 'i');
            const fontPattern = new RegExp(${fontPatternSource});

            function isColor(value) {
                return typeof value === 'string' && colorPattern.test(value);
            }

            function isSafeFont(value) {
                return typeof value === 'string' && fontPattern.test(value);
            }

            function isThemePayload(value) {
                if (!isRecord(value) || !hasExactKeys(value, payloadKeys)) return false;
                if (typeof value.enabled !== 'boolean') return false;
                if (!isColor(value.background) || !isColor(value.foreground)) return false;
                if (!isSafeFont(value.fontText) || !isSafeFont(value.fontMono)) return false;
                if (!isRecord(value.roles) || !hasExactKeys(value.roles, roleSchemaKeys)) return false;
                for (let i = 0; i < roleKeys.length; i++) {
                    if (!isColor(value.roles[roleKeys[i]])) return false;
                }
                return true;
            }

            function isThemeUpdate(value) {
                return isRecord(value)
                    && hasExactKeys(value, ['payload', 'type'])
                    && value.type === 'theme-update'
                    && isThemePayload(value.payload);
            }

            function isThemeContextRequest(value) {
                return isRecord(value)
                    && hasExactKeys(value, ['type'])
                    && value.type === 'theme-context-request';
            }

            function isThemeReady(value) {
                return isRecord(value)
                    && hasExactKeys(value, ['type'])
                    && value.type === 'raven-quarto-theme-ready';
            }

            function postBridgePing() {
                if (!frame || !frame.contentWindow || !frameOrigin) return;
                frame.contentWindow.postMessage({
                    type: 'raven-quarto-theme-ping'
                }, frameOrigin);
            }

            function syncThemeButton() {
                themeButton.setAttribute('aria-pressed', themeEnabled ? 'true' : 'false');
                themeButton.hidden = currentState.kind !== 'serving';
                themeButton.title = bridgeAvailable === false
                    ? "VS Code theme can't apply to this page"
                    : 'Apply VS Code theme';
            }

            function postThemeToFrame() {
                if (!frame || !frame.contentWindow || !currentTheme || !frameOrigin) return;
                frame.contentWindow.postMessage({
                    __ravenQuartoTheme: true,
                    ...currentTheme
                }, frameOrigin);
            }

            function reportThemeContext() {
                try {
                    const rootStyle = getComputedStyle(document.documentElement);
                    const bodyStyle = getComputedStyle(document.body);
                    const background = (
                        rootStyle.getPropertyValue('--vscode-editor-background')
                        || bodyStyle.backgroundColor
                        || rootStyle.backgroundColor
                        || ''
                    ).trim();
                    if (background) {
                        vscode.postMessage({ type: 'theme-context', background });
                    }
                } catch (_) { /* host falls back to the first candidate */ }
            }

            function clearBlankAdvisory() {
                if (blankTimer !== null) window.clearTimeout(blankTimer);
                blankTimer = null;
                banner.textContent = '';
                banner.hidden = true;
            }

            function markBridgeAvailable() {
                // Unlike iframe load, this origin-pinned handshake proves that
                // the framed HTML is alive. It is therefore safe to cancel or
                // dismiss the otherwise deliberately conservative advisory.
                clearBlankAdvisory();
                if (bridgeTimer !== null) window.clearTimeout(bridgeTimer);
                bridgeTimer = null;
                if (bridgeAvailable !== true) {
                    bridgeAvailable = true;
                    vscode.postMessage({ type: 'theme-bridge-status', available: true });
                }
                syncThemeButton();
            }

            function armBridgeTimeout() {
                if (bridgeTimer !== null) window.clearTimeout(bridgeTimer);
                bridgeAvailable = null;
                syncThemeButton();
                bridgeTimer = window.setTimeout(function () {
                    bridgeTimer = null;
                    bridgeAvailable = false;
                    syncThemeButton();
                    vscode.postMessage({ type: 'theme-bridge-status', available: false });
                }, 3000);
            }

            function render(next) {
                currentState = next;
                status.textContent = '';
                status.hidden = next.kind === 'serving';
                openButton.hidden = next.kind !== 'serving';
                stopButton.hidden = next.kind !== 'starting' && next.kind !== 'serving';
                restartButton.hidden = next.kind === 'starting' || next.kind === 'serving';
                themeButton.hidden = next.kind !== 'serving';

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
            themeButton.addEventListener('click', function () {
                themeEnabled = !themeEnabled;
                syncThemeButton();
                if (currentTheme) {
                    currentTheme = { ...currentTheme, enabled: themeEnabled };
                    postThemeToFrame();
                }
                vscode.postMessage({ type: 'theme-changed', applied: themeEnabled });
            });
            window.addEventListener('message', function (event) {
                const message = event.data;
                // Branch A: a framed Quarto page can only complete the exact
                // ready handshake from its pinned serving origin. Always
                // return so frame data can never fall through to host actions.
                if (frame && event.source === frame.contentWindow) {
                    if (event.origin === frameOrigin && isThemeReady(message)) {
                        markBridgeAvailable();
                    }
                    return;
                }

                // Branch B: extension-host delivery. This branch is disjoint
                // from the frame branch above; do not combine their origins.
                if (event.origin === '' || event.origin === window.origin) {
                    if (isStateUpdate(message)) {
                        render(message.payload);
                        return;
                    }
                    if (isThemeUpdate(message)) {
                        currentTheme = message.payload;
                        themeEnabled = currentTheme.enabled;
                        syncThemeButton();
                        postThemeToFrame();
                        return;
                    }
                    if (isThemeContextRequest(message)) {
                        reportThemeContext();
                    }
                    return;
                }
                return;
            });

            render(initial.state);
            syncThemeButton();
            reportThemeContext();
            if (frame) {
                blankTimer = window.setTimeout(function () {
                    blankTimer = null;
                    banner.textContent = 'If the preview looks blank, try Open in Browser.';
                    banner.hidden = false;
                    vscode.postMessage({ type: 'load-timeout' });
                }, 8000);
                frame.addEventListener('load', function () {
                    armBridgeTimeout();
                    postBridgePing();
                    postThemeToFrame();
                });
            }
            vscode.postMessage({ type: 'webview-ready' });
        }());
    </script>
</body>
</html>`;
}
