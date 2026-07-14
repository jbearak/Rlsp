/**
 * Pure wire protocol between the Quarto preview host and outer webview.
 *
 * The webview is a trust boundary. Both validators enforce per-variant
 * exact key sets at every object level, rejecting missing fields and
 * payload smuggling through extra properties instead of silently ignoring
 * them. This module has no VS Code or DOM imports so Bun can test the same
 * guards used by the extension host and shell.
 */

export type QuartoPreviewViewState =
    | { kind: 'starting' }
    | { kind: 'serving'; externalUrl: string }
    | { kind: 'failed'; detailText: string }
    | { kind: 'exited-unexpectedly'; code: number | null }
    | { kind: 'stopped' }
    | { kind: 'restore-placeholder' };

export type ExtensionToPreviewMessage =
    | { type: 'state-update'; payload: QuartoPreviewViewState };

export type PreviewToExtensionMessage =
    | { type: 'webview-ready' }
    | { type: 'open-in-browser' }
    | { type: 'stop-preview' }
    | { type: 'request-restart' }
    | { type: 'load-timeout' }
    | { type: 'report-error'; message: string };

const PREVIEW_MESSAGE_SCHEMAS: Record<PreviewToExtensionMessage['type'], readonly string[]> = {
    'webview-ready': ['type'],
    'open-in-browser': ['type'],
    'stop-preview': ['type'],
    'request-restart': ['type'],
    'load-timeout': ['type'],
    'report-error': ['message', 'type'],
};

const VIEW_STATE_SCHEMAS: Record<QuartoPreviewViewState['kind'], readonly string[]> = {
    'starting': ['kind'],
    'serving': ['externalUrl', 'kind'],
    'failed': ['detailText', 'kind'],
    'exited-unexpectedly': ['code', 'kind'],
    'stopped': ['kind'],
    'restore-placeholder': ['kind'],
};

function isRecord(value: unknown): value is Record<string, unknown> {
    return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function hasExactKeys(value: Record<string, unknown>, expected: readonly string[]): boolean {
    const actual = Object.keys(value).sort();
    if (actual.length !== expected.length) return false;
    for (let i = 0; i < expected.length; i++) {
        if (actual[i] !== expected[i]) return false;
    }
    return true;
}

function isViewState(value: unknown): value is QuartoPreviewViewState {
    if (!isRecord(value) || typeof value.kind !== 'string') return false;
    if (!Object.prototype.hasOwnProperty.call(VIEW_STATE_SCHEMAS, value.kind)) return false;
    const expected = VIEW_STATE_SCHEMAS[value.kind as QuartoPreviewViewState['kind']];
    if (!hasExactKeys(value, expected)) return false;

    switch (value.kind) {
        case 'serving':
            return typeof value.externalUrl === 'string';
        case 'failed':
            return typeof value.detailText === 'string';
        case 'exited-unexpectedly':
            return value.code === null ||
                (typeof value.code === 'number' && Number.isInteger(value.code));
        default:
            return true;
    }
}

/** Strictly validate a host-to-preview state update. */
export function isExtensionToPreviewMessage(value: unknown): value is ExtensionToPreviewMessage {
    if (!isRecord(value)) return false;
    if (!hasExactKeys(value, ['payload', 'type'])) return false;
    return value.type === 'state-update' && isViewState(value.payload);
}

/** Strictly validate a preview-to-host action or error report. */
export function isPreviewToExtensionMessage(value: unknown): value is PreviewToExtensionMessage {
    if (!isRecord(value) || typeof value.type !== 'string') return false;
    if (!Object.prototype.hasOwnProperty.call(PREVIEW_MESSAGE_SCHEMAS, value.type)) return false;
    const expected = PREVIEW_MESSAGE_SCHEMAS[value.type as PreviewToExtensionMessage['type']];
    if (!hasExactKeys(value, expected)) return false;
    return value.type !== 'report-error' || typeof value.message === 'string';
}
