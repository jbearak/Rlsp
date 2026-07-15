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

export const QUARTO_THEME_ROLE_KEYS = [
    'keyword',
    'string',
    'number',
    'comment',
    'function',
    'type',
    'variable',
    'operator',
    'punctuation',
    'constant',
] as const;

export type QuartoThemeRole = typeof QUARTO_THEME_ROLE_KEYS[number];

export interface QuartoThemePayload {
    enabled: boolean;
    background: string;
    foreground: string;
    roles: Record<QuartoThemeRole, string>;
    fontText: string;
    fontMono: string;
}

/** Theme delivery from Raven's trusted outer shell into the Quarto page. */
export type RavenQuartoThemeMessage = QuartoThemePayload & {
    __ravenQuartoTheme: true;
};

/** Contentless handshake from the injected Quarto bridge to its parent shell. */
export interface RavenQuartoThemeReadyMessage {
    type: 'raven-quarto-theme-ready';
}

/** Contentless shell-to-page availability probe, sent after every frame load. */
export interface RavenQuartoThemePingMessage {
    type: 'raven-quarto-theme-ping';
}

export type ExtensionToPreviewMessage =
    | { type: 'state-update'; payload: QuartoPreviewViewState }
    | { type: 'theme-update'; payload: QuartoThemePayload }
    | { type: 'theme-context-request' };

export type PreviewToExtensionMessage =
    | { type: 'webview-ready' }
    | { type: 'open-in-browser' }
    | { type: 'stop-preview' }
    | { type: 'request-restart' }
    | { type: 'load-timeout' }
    | { type: 'report-error'; message: string }
    | { type: 'theme-context'; background: string }
    | { type: 'theme-changed'; applied: boolean }
    | { type: 'theme-bridge-status'; available: boolean };

const PREVIEW_MESSAGE_SCHEMAS: Record<PreviewToExtensionMessage['type'], readonly string[]> = {
    'webview-ready': ['type'],
    'open-in-browser': ['type'],
    'stop-preview': ['type'],
    'request-restart': ['type'],
    'load-timeout': ['type'],
    'report-error': ['message', 'type'],
    'theme-context': ['background', 'type'],
    'theme-changed': ['applied', 'type'],
    'theme-bridge-status': ['available', 'type'],
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

export const QUARTO_THEME_PAYLOAD_KEYS = [
    'background',
    'enabled',
    'fontMono',
    'fontText',
    'foreground',
    'roles',
] as const;

const THEME_ROLE_SCHEMA_KEYS = [...QUARTO_THEME_ROLE_KEYS].sort();

export const RAVEN_QUARTO_THEME_MESSAGE_KEYS = [
    '__ravenQuartoTheme',
    ...QUARTO_THEME_PAYLOAD_KEYS,
] as const;

/** Shared regex source used by the host validator and generated shell. */
export const QUARTO_CSS_COLOR_PATTERN_SOURCE = String.raw`^(?:#[0-9a-f]{3,4}|#[0-9a-f]{6}(?:[0-9a-f]{2})?|rgb\(\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*\)|rgba\(\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*,\s*\d{1,3}%?\s*,\s*(?:0|1|0?\.\d+|\d{1,3}%)\s*\))$`;

/**
 * Shared character guard for values emitted by `sanitizeFontFamily`.
 *
 * This deliberately matches the settings schema and sanitizer's character
 * ban instead of rejecting every C0 control character. The producer performs
 * the stronger structural validation (quotes, comments, commas, and parens).
 */
export const QUARTO_SANITIZED_FONT_PATTERN_SOURCE =
    String.raw`^[^;{}<>\\\n\r\t\f\v\0]*$`;

const CSS_COLOR = new RegExp(QUARTO_CSS_COLOR_PATTERN_SOURCE, 'i');
const SANITIZED_FONT = new RegExp(QUARTO_SANITIZED_FONT_PATTERN_SOURCE);

/** Strictly validate theme data before it can enter a CSS declaration. */
export function isQuartoThemePayload(value: unknown): value is QuartoThemePayload {
    if (!isRecord(value) || !hasExactKeys(value, QUARTO_THEME_PAYLOAD_KEYS)) return false;
    if (typeof value.enabled !== 'boolean') return false;
    if (!isColor(value.background) || !isColor(value.foreground)) return false;
    if (typeof value.fontText !== 'string' || !SANITIZED_FONT.test(value.fontText)) return false;
    if (typeof value.fontMono !== 'string' || !SANITIZED_FONT.test(value.fontMono)) return false;
    const roles = value.roles;
    if (!isRecord(roles) || !hasExactKeys(roles, THEME_ROLE_SCHEMA_KEYS)) return false;
    return QUARTO_THEME_ROLE_KEYS.every((role) => isColor(roles[role]));
}

/** Strictly validate shell-to-page bridge delivery, including its marker. */
export function isRavenQuartoThemeMessage(value: unknown): value is RavenQuartoThemeMessage {
    if (!isRecord(value) || !hasExactKeys(value, RAVEN_QUARTO_THEME_MESSAGE_KEYS)) return false;
    if (value.__ravenQuartoTheme !== true) return false;
    return isQuartoThemePayload({
        enabled: value.enabled,
        background: value.background,
        foreground: value.foreground,
        roles: value.roles,
        fontText: value.fontText,
        fontMono: value.fontMono,
    });
}

/** Strictly validate the contentless page-to-shell ready handshake. */
export function isRavenQuartoThemeReadyMessage(
    value: unknown,
): value is RavenQuartoThemeReadyMessage {
    return isRecord(value) &&
        hasExactKeys(value, ['type']) &&
        value.type === 'raven-quarto-theme-ready';
}

/** Strictly validate the contentless shell-to-page availability probe. */
export function isRavenQuartoThemePingMessage(
    value: unknown,
): value is RavenQuartoThemePingMessage {
    return isRecord(value) &&
        hasExactKeys(value, ['type']) &&
        value.type === 'raven-quarto-theme-ping';
}

function isColor(value: unknown): value is string {
    return typeof value === 'string' && CSS_COLOR.test(value);
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
    if (!isRecord(value) || typeof value.type !== 'string') return false;
    switch (value.type) {
        case 'state-update':
            return hasExactKeys(value, ['payload', 'type']) && isViewState(value.payload);
        case 'theme-update':
            return hasExactKeys(value, ['payload', 'type']) &&
                isQuartoThemePayload(value.payload);
        case 'theme-context-request':
            return hasExactKeys(value, ['type']);
        default:
            return false;
    }
}

/** Strictly validate a preview-to-host action or error report. */
export function isPreviewToExtensionMessage(value: unknown): value is PreviewToExtensionMessage {
    if (!isRecord(value) || typeof value.type !== 'string') return false;
    if (!Object.prototype.hasOwnProperty.call(PREVIEW_MESSAGE_SCHEMAS, value.type)) return false;
    const expected = PREVIEW_MESSAGE_SCHEMAS[value.type as PreviewToExtensionMessage['type']];
    if (!hasExactKeys(value, expected)) return false;
    switch (value.type) {
        case 'report-error':
            return typeof value.message === 'string';
        case 'theme-context':
            return typeof value.background === 'string';
        case 'theme-changed':
            return typeof value.applied === 'boolean';
        case 'theme-bridge-status':
            return typeof value.available === 'boolean';
        default:
            return true;
    }
}
