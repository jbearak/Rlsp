import * as path from 'path';
import { parseRenderedOutputPath } from './output-path';
import { githubDark, githubLight, type GithubPalette } from './github-colors';

export type KnitOutputMessage =
    | { type: 'refresh' }
    | { type: 'openInBrowser' }
    | { type: 'themeChanged'; applied: boolean }
    | { type: 'themeContext'; editorBackground: string }
    /**
     * The webview just finished booting (initial load or panel reuse).
     * Asks the host to (re-)resolve and push the current VS Code theme
     * palette. Needed for the panel-reuse path: setting
     * `panel.webview.html` discards the prior document and its message
     * listener; a synchronous postMessage from the host can race the
     * fresh shell's `addEventListener('message')` and be silently
     * dropped. Having the new shell pull instead avoids the race.
     */
    | { type: 'requestPalette' }
    /**
     * Same pull pattern as `requestPalette`, but for the live font
     * override. The webview asks the host to re-resolve the user's
     * font settings and push `__ravenFontFamilies`, so the iframe-
     * rebuild race after `panel.webview.html = …` cannot leave the
     * webview without the override that the on-disk `.html` no
     * longer carries (the baked declarations are still there, but the
     * webview's <style> override always wins on the cascade).
     */
    | { type: 'requestFonts' }
    /**
     * The user picked a format from the webview's Export ▾ popover.
     * The format choice now crosses the trust boundary — it's
     * validated strictly against the `EXPORT_FORMATS` whitelist in
     * `isKnitOutputMessage` before the host dispatches the matching
     * `raven.knit.export*` command. The webview owns the menu UI
     * (an HTML popover, same pattern as the plot viewer's share
     * popover) instead of routing through VS Code's native
     * QuickPick — the popover never raises the toolbar height and
     * dismisses on outside click or Escape via the platform popover
     * API.
     */
    | { type: 'requestExport'; format: ExportFormat }
    /**
     * The user clicked the toolbar's Export button while an export
     * was already in flight (the button doubles as a cancel control
     * when spinning). Host looks up the source's current controller
     * and calls `cancel()`.
     */
    | { type: 'cancelExport' };

/**
 * Whitelist of accepted `requestExport` formats. Defined as a const
 * tuple so `ExportFormat` is the union of literal strings and so the
 * webview-side trust-boundary check can reuse the same list at
 * validation time. Adding a new export format means extending this
 * tuple AND adding a `case` to `KnitOutputPanel.handleMessage`'s
 * dispatch table — the type system catches both halves.
 */
export const EXPORT_FORMATS = ['html', 'pdf', 'docx'] as const;
export type ExportFormat = typeof EXPORT_FORMATS[number];

/**
 * Per-type allowed key set for the trust-boundary validator. Sorted so
 * we can compare against `Object.keys(msg).sort()` for exact-schema
 * equality. Adding a new message type means adding it here AND adding a
 * value-type check below in `isKnitOutputMessage`.
 */
const MESSAGE_SCHEMAS: Record<KnitOutputMessage['type'], readonly string[]> = {
    refresh: ['type'],
    openInBrowser: ['type'],
    themeChanged: ['applied', 'type'],
    themeContext: ['editorBackground', 'type'],
    requestPalette: ['type'],
    requestFonts: ['type'],
    requestExport: ['format', 'type'],
    cancelExport: ['type'],
};

/**
 * Marker key on `postMessage` payloads from the extension host to the
 * outer-shell script. We discriminate via a property instead of a
 * `type:` field because the existing in-iframe probe path also uses
 * `event.data.__ravenKnitProbe`, and a shared discriminator field
 * would risk silent collision with that or with VS Code-injected
 * message shapes.
 */
export const VSCODE_PALETTE_UPDATE_KEY = '__ravenVscodeThemePalette';

export interface VscodeThemePaletteUpdate {
    /** Marker — must equal `true`. */
    __ravenVscodeThemePalette: true;
    /**
     * Pre-built CSS declarations matching `paletteCssDeclarations` for
     * the VS Code theme. `null` clears the override so the toggle
     * falls back to the GitHub variant.
     */
    css: string | null;
}

/**
 * Marker key for the live-font override channel. Same shape and trust
 * boundary as `VSCODE_PALETTE_UPDATE_KEY` (host posts a complete CSS
 * declaration string, webview's accept-regex enforces the exact
 * declaration shape before injecting). Distinct key so the webview can
 * tell theme updates from font updates and route each into its own
 * `<style>` element — clobbering between channels would lose either
 * theme colors or fonts.
 */
export const FONT_FAMILIES_UPDATE_KEY = '__ravenFontFamilies';

export interface FontFamiliesUpdate {
    /** Marker — must equal `true`. */
    __ravenFontFamilies: true;
    /**
     * Pre-built CSS declarations matching `fontsCssDeclarations`:
     * `--raven-font-text: …; --raven-font-mono: …;` — both declarations,
     * in that order, separated by a single space. `null` clears any
     * prior override so the iframe falls back to the fonts baked into
     * the on-disk `.html`.
     */
    css: string | null;
}

/**
 * Marker key for the host→webview export-busy channel. When the host
 * starts an export op for this panel's source, it posts `{ busy: true }`;
 * when the op ends (done / cancelled / failed) it posts `{ busy: false }`.
 * The webview script toggles `exportBtn.dataset.busy` so the next click
 * dispatches `cancelExport` instead of `requestExport`. The payload is a
 * single boolean — any other shape is rejected at the trust boundary.
 */
export const EXPORT_BUSY_UPDATE_KEY = '__ravenExportBusy';

export interface ExportBusyUpdate {
    /** Marker — must equal `true`. */
    __ravenExportBusy: true;
    /** True while an export op is in flight for this panel's source. */
    busy: boolean;
}

/**
 * Build the CSS variable declarations for the live-font override. Same
 * shape `render-html.ts:fontsAsCssVars` emits when baking the rendered
 * document, joined with a single space so the whole payload fits on one
 * line — the webview's accept-regex (`RAVEN_FONT_CSS_RE` in
 * `buildShellHtml`) checks for that exact shape as the trust boundary
 * between extension-host string assembly and `style.textContent`
 * injection.
 *
 * Inputs are trusted: callers MUST pass the output of
 * `resolveFontFamilies` (which sanitizes every candidate). Anything
 * else can corrupt the iframe stylesheet — see
 * `render-html.ts:sanitizeFontFamily` for the threat model.
 */
export function fontsCssDeclarations(fonts: { text: string; mono: string }): string {
    return `--raven-font-text: ${fonts.text}; --raven-font-mono: ${fonts.mono};`;
}

/**
 * Strict type-narrowing for messages posted from the Knit Preview webview.
 *
 * The webview is a trust boundary. We use **per-type exact-schema
 * matching**: the message object's keys must equal the declared key set
 * for that type, no more and no less. This rejects payload smuggling
 * via extra fields (e.g., `{ type: 'requestExport', format: '../etc/passwd' }`)
 * without silently ignoring them, AND still accepts the legitimate
 * payload-carrying messages like `themeChanged` / `themeContext`.
 */
export function isKnitOutputMessage(msg: unknown): msg is KnitOutputMessage {
    if (msg === null || typeof msg !== 'object') return false;
    const obj = msg as Record<string, unknown>;
    if (typeof obj.type !== 'string') return false;
    const expected = MESSAGE_SCHEMAS[obj.type as KnitOutputMessage['type']];
    if (!expected) return false;
    const actual = Object.keys(obj).sort();
    if (actual.length !== expected.length) return false;
    for (let i = 0; i < expected.length; i++) {
        if (actual[i] !== expected[i]) return false;
    }
    // Per-type value-type checks for non-`type` fields.
    if (obj.type === 'themeChanged' && typeof obj.applied !== 'boolean') return false;
    if (obj.type === 'themeContext' && typeof obj.editorBackground !== 'string') return false;
    if (obj.type === 'requestExport') {
        if (typeof obj.format !== 'string') return false;
        if (!EXPORT_FORMATS.includes(obj.format as ExportFormat)) return false;
    }
    return true;
}

/**
 * Possible outcomes of a single `runKnit` invocation, after we have
 * classified the raw engine result. Discriminated by `kind`. No user-
 * facing toasts or webview operations have been performed yet — that
 * happens in `renderOutcome`, OUTSIDE the `withProgress` callback. This
 * is the core of the Piece A bug fix: keeping the `withProgress`
 * lifecycle short and predictable.
 */
export type KnitOutcome =
    | { kind: 'spawnError'; error: NodeJS.ErrnoException }
    | { kind: 'cancelled' }
    | { kind: 'timedOut'; timeoutMs?: number }
    | { kind: 'failed'; exitCode: number | null }
    | { kind: 'noOutput' }
    | { kind: 'ok'; parsedOutputs: string[]; cwd: string | undefined };

/** Minimal subset of `runKnit`'s return value classify needs. */
export interface ClassifyInput {
    spawnError: NodeJS.ErrnoException | null;
    cancelled: boolean;
    timedOut: boolean;
    exitCode: number | null;
    stdout: string;
    stderr: string;
}

/**
 * Pure classifier mapping the engine's raw result onto a KnitOutcome.
 * Branch priority mirrors the original runKnitCommand:
 *   spawnError > cancelled > timedOut > failed > noOutput / ok
 */
export function classify(
    result: ClassifyInput,
    ctx: { cwd: string | undefined },
): KnitOutcome {
    if (result.spawnError) return { kind: 'spawnError', error: result.spawnError };
    if (result.cancelled) return { kind: 'cancelled' };
    if (result.timedOut) return { kind: 'timedOut' };
    if (result.exitCode !== 0) return { kind: 'failed', exitCode: result.exitCode };
    const parsed = parseRenderedOutputPath(result.stdout + '\n' + result.stderr).paths;
    if (parsed.length === 0) return { kind: 'noOutput' };
    return { kind: 'ok', parsedOutputs: parsed, cwd: ctx.cwd };
}

/**
 * Build the CSS variable declarations for a single GitHub palette
 * variant, in the same shape `render-html.ts:paletteAsCssVars` emits
 * when baking the rendered document. Used by the theme overlay to
 * re-emit `--raven-bg` / `--raven-fg` / `--raven-c-*` at overlay time
 * so the syntax-highlight palette tracks VS Code theme switches in
 * lockstep with the code-block background. Without this re-emit, a
 * theme switch after knit would update `--vscode-textCodeBlock-
 * background` (resolved live from the outer shell) while leaving the
 * baked-at-knit `--raven-c-*` tokens on the original variant — e.g.
 * dark tokens on a light shade.
 */
export function paletteCssDeclarations(palette: GithubPalette): string {
    return [
        `--raven-bg: ${palette.background};`,
        `--raven-fg: ${palette.foreground};`,
        `--raven-c-keyword: ${palette.roles.keyword};`,
        `--raven-c-string: ${palette.roles.string};`,
        `--raven-c-number: ${palette.roles.number};`,
        `--raven-c-comment: ${palette.roles.comment};`,
        `--raven-c-function: ${palette.roles.function};`,
        `--raven-c-type: ${palette.roles.type};`,
        `--raven-c-variable: ${palette.roles.variable};`,
        `--raven-c-operator: ${palette.roles.operator};`,
        `--raven-c-punctuation: ${palette.roles.punctuation};`,
        `--raven-c-constant: ${palette.roles.constant};`,
    ].join(' ');
}

/**
 * Inlined codicon SVGs (from @vscode/codicons, MIT). Kept inline so the
 * toolbar has no runtime font dependency and the outer-shell CSP can
 * stay locked down (`font-src` need not whitelist a codicon woff URL).
 * `fill="currentColor"` makes each icon inherit the surrounding
 * button's foreground color so they track theme switches without
 * extra CSS.
 *
 * SECURITY INVARIANT (mirrors the plot viewer's App.svelte): these
 * strings are injected into the outer-shell HTML verbatim, so they
 * MUST stay pure hand-vetted SVG — no `<script>`, `<use>`, `<image>`,
 * `<a>`, `<foreignObject>`, `<iframe>`, event-handler attributes
 * (`onclick=`, `onload=`, …), or `href`/`xlink:href`. New icons go
 * through the same review.
 */
const ICON_REFRESH =
    '<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor" aria-hidden="true" focusable="false"><path d="M3 8C3 5.23858 5.23858 3 8 3C9.63527 3 11.0878 3.78495 12.0005 5H10C9.72386 5 9.5 5.22386 9.5 5.5C9.5 5.77614 9.72386 6 10 6H12.8904C12.8973 6.00014 12.9041 6.00014 12.911 6H13C13.2761 6 13.5 5.77614 13.5 5.5V2.5C13.5 2.22386 13.2761 2 13 2C12.7239 2 12.5 2.22386 12.5 2.5V4.03138C11.4009 2.78613 9.79253 2 8 2C4.68629 2 2 4.68629 2 8C2 11.3137 4.68629 14 8 14C11.1301 14 13.6999 11.6035 13.9756 8.54488C14.0003 8.26985 13.7975 8.0268 13.5225 8.00202C13.2474 7.97723 13.0044 8.1801 12.9796 8.45512C12.75 11.003 10.6079 13 8 13C5.23858 13 3 10.7614 3 8Z"/></svg>';
const ICON_GLOBE =
    '<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor" aria-hidden="true" focusable="false"><path d="M8 1C4.141 1 1 4.141 1 8C1 11.859 4.141 15 8 15C11.859 15 15 11.859 15 8C15 4.141 11.859 1 8 1ZM8 14C7.422 14 6.686 12.906 6.288 11H9.713C9.315 12.906 8.579 14 8.001 14H8ZM6.121 10C6.044 9.392 6 8.723 6 8C6 7.277 6.044 6.608 6.121 6H9.878C9.955 6.608 9.999 7.277 9.999 8C9.999 8.723 9.955 9.392 9.878 10H6.121ZM2 8C2 7.299 2.121 6.626 2.343 6H5.121C5.041 6.656 5 7.332 5 8C5 8.668 5.041 9.344 5.121 10H2.343C2.121 9.374 2 8.701 2 8ZM8 2C8.578 2 9.314 3.094 9.712 5H6.287C6.685 3.094 7.422 2 8 2ZM10.879 6H13.657C13.879 6.626 14 7.299 14 8C14 8.701 13.879 9.374 13.657 10H10.879C10.959 9.344 11 8.668 11 8C11 7.332 10.959 6.656 10.879 6ZM13.195 5H10.722C10.516 3.938 10.199 2.98 9.775 2.268C11.228 2.719 12.446 3.707 13.195 5ZM6.226 2.268C5.802 2.98 5.484 3.938 5.279 5H2.806C3.556 3.707 4.774 2.718 6.226 2.268ZM2.805 11H5.278C5.484 12.062 5.801 13.02 6.225 13.732C4.772 13.281 3.554 12.293 2.805 11ZM9.774 13.732C10.198 13.02 10.516 12.062 10.721 11H13.194C12.444 12.293 11.226 13.282 9.774 13.732Z"/></svg>';
// codicon "link-external" share glyph — the same icon the plot viewer
// toolbar uses for its Copy / PNG / SVG / PDF popover. Reusing the
// glyph across surfaces keeps "click for a menu of export options"
// recognizable everywhere it appears.
const ICON_SHARE =
    '<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor" aria-hidden="true" focusable="false"><path d="M11.307 1.10533C11.1562 0.988085 10.9519 0.966945 10.7803 1.05085C10.6088 1.13475 10.5 1.30904 10.5 1.5V3.49274C10.4571 3.49456 10.4122 3.49701 10.3654 3.5002C9.96247 3.52766 9.41128 3.61105 8.82119 3.83704C8.11343 4.10809 7.34877 4.58508 6.72601 5.41126C6.10338 6.23727 5.64499 7.38259 5.50206 8.95474C5.48301 9.16438 5.5973 9.36351 5.78793 9.4528C5.97857 9.54209 6.20471 9.50241 6.35356 9.35356C7.54248 8.16464 8.72298 7.57773 9.59562 7.28685C9.9558 7.16679 10.2643 7.09693 10.5 7.0563V9C10.5 9.1969 10.6156 9.37546 10.7952 9.45612C10.9748 9.53678 11.185 9.50452 11.3322 9.37371L15.8322 5.37371C15.9432 5.27502 16.0046 5.13207 15.9997 4.98361C15.9949 4.83514 15.9242 4.69653 15.807 4.60533L11.307 1.10533ZM10.9429 4.49679L10.9457 4.49705C11.0865 4.51223 11.2279 4.46706 11.3335 4.37257C11.4394 4.27772 11.5 4.14223 11.5 4V2.52232L14.7186 5.02564L11.5 7.88658V6.5C11.5 6.22386 11.2762 6 11 6L10.9989 6L10.9976 6.00001L10.9943 6.00003L10.9848 6.00014L10.9552 6.00087C10.9307 6.00166 10.897 6.00316 10.8544 6.00599C10.7695 6.01166 10.6495 6.02268 10.4996 6.04409C10.1999 6.08691 9.77971 6.17139 9.2794 6.33816C8.55493 6.57965 7.66479 6.99299 6.7319 7.69863C6.9264 6.98158 7.2077 6.43355 7.52456 6.01319C8.01593 5.36132 8.61523 4.98675 9.17883 4.7709C9.65371 4.58903 10.1025 4.52044 10.4334 4.49788C10.5981 4.48666 10.7314 4.48699 10.8211 4.48988C10.866 4.49133 10.8997 4.49341 10.9209 4.49498L10.9429 4.49679ZM3.5 2C2.11929 2 1 3.11929 1 4.5V12.5C1 13.8807 2.11929 15 3.5 15H11.5C12.8807 15 14 13.8807 14 12.5V9.5C14 9.22386 13.7761 9 13.5 9C13.2239 9 13 9.22386 13 9.5V12.5C13 13.3284 12.3284 14 11.5 14H3.5C2.67157 14 2 13.3284 2 12.5V4.5C2 3.67157 2.67157 3 3.5 3H7.5C7.77614 3 8 2.77614 8 2.5C8 2.22386 7.77614 2 7.5 2H3.5Z"/></svg>';
const ICON_STOP =
    '<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor" aria-hidden="true" focusable="false"><path d="M6 5C5.44772 5 5 5.44772 5 6V10C5 10.5523 5.44772 11 6 11H10C10.5523 11 11 10.5523 11 10V6C11 5.44772 10.5523 5 10 5H6ZM1 8C1 4.13401 4.13401 1 8 1C11.866 1 15 4.13401 15 8C15 11.866 11.866 15 8 15C4.13401 15 1 11.866 1 8ZM8 2C4.68629 2 2 4.68629 2 8C2 11.3137 4.68629 14 8 14C11.3137 14 14 11.3137 14 8C14 4.68629 11.3137 2 8 2Z"/></svg>';
const ICON_SYMBOL_COLOR =
    '<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor" aria-hidden="true" focusable="false"><path d="M8.00101 1C4.13401 1 1.00101 3.8 1.00101 7.667C1.00101 8.956 2.04501 10 3.33401 10C4.75101 10 4.72101 9 6.00001 9C6.64401 9 7.00001 9.606 7.00001 10.25V11.5C7.00001 13.433 8.56701 15 10.5 15C13.653 15 14.999 11.215 14.999 8C14.999 4.134 11.866 1 8.00001 1H8.00101ZM10.5 14C9.12201 14 8.00001 12.878 8.00001 11.5V10.25C8.00001 8.967 7.14001 8 6.00001 8C5.04001 8 4.49801 8.412 4.13901 8.685C3.85401 8.902 3.72401 9 3.33401 9C2.59901 9 2.00101 8.402 2.00101 7.667C2.00101 4.436 4.58001 2 8.00101 2C11.309 2 14 4.692 14 8C14 10.412 13.068 14 10.501 14H10.5ZM12 11C12 11.552 11.552 12 11 12C10.448 12 10 11.552 10 11C10 10.448 10.448 10 11 10C11.552 10 12 10.448 12 11ZM13 8C13 8.552 12.552 9 12 9C11.448 9 11 8.552 11 8C11 7.448 11.448 7 12 7C12.552 7 13 7.448 13 8ZM6.00001 5C6.00001 5.552 5.55201 6 5.00001 6C4.44801 6 4.00001 5.552 4.00001 5C4.00001 4.448 4.44801 4 5.00001 4C5.55201 4 6.00001 4.448 6.00001 5ZM10 5C10 4.448 10.448 4 11 4C11.552 4 12 4.448 12 5C12 5.552 11.552 6 11 6C10.448 6 10 5.552 10 5ZM9.00001 4C9.00001 4.552 8.55201 5 8.00001 5C7.44801 5 7.00001 4.552 7.00001 4C7.00001 3.448 7.44801 3 8.00001 3C8.55201 3 9.00001 3.448 9.00001 4Z"/></svg>';

/**
 * Minimal vscode.Webview shape buildShellHtml needs. Defined inline so
 * the pure helper has no dependency on the actual vscode module — tests
 * pass a fake.
 */
function escapeHtml(s: string): string {
    return s
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

/**
 * Serialize a (possibly user-controlled) string to a JS literal safe to
 * embed inside an inline `<script>`. `JSON.stringify` does not escape
 * `<`, so a value containing `</script>` (legal in a filesystem path)
 * would otherwise terminate the script element early — both a breakage
 * and an HTML-injection vector. Replacing each `<` with its `<` JS
 * unicode escape keeps the literal valid JS while neutralizing
 * `</script>` and `<!--`.
 */
function jsonForScript(value: string): string {
    return JSON.stringify(value).replace(/</g, '\\u003c');
}

/**
 * Build the outer-shell HTML for the Knit Preview webview.
 *
 * The shell is Raven-controlled and owns the CSP in `<head>`; the
 * rendered HTML loads inside `<iframe srcdoc="..." sandbox="allow-
 * same-origin">`. Three independent containment layers (sandbox
 * attribute, outer-shell CSP, `localResourceRoots`) make the security
 * model robust to either layer failing.
 *
 * Why `srcdoc` rather than `src`: a nested `<iframe>` inside a VS Code
 * webview cannot navigate to a `webview.asWebviewUri(...)` URL —
 * Electron's resource handler does not intercept the nested-frame
 * navigation, so the network stack tries DNS resolution on
 * `file+.vscode-resource.vscode-cdn.net` and fails with
 * `ERR_NAME_NOT_RESOLVED`. Inlining the HTML via `srcdoc` avoids the
 * URL navigation entirely; relative subresource URLs in the rendered
 * HTML resolve via the injected `<base href="...">` (which IS a
 * subresource request, and those go through the SW happily).
 *
 * `sandbox="allow-same-origin"` is required (rather than `sandbox=""`)
 * so the srcdoc document inherits the parent webview origin instead of
 * a unique opaque origin. Scripts, forms, popups, and top-navigation
 * remain blocked.
 *
 * Pure helper — no dependency on the vscode module. The caller
 * (`KnitOutputPanel`) reads the rendered HTML from disk and converts
 * the output's parent directory via `webview.asWebviewUri(...)`,
 * passing the results as `htmlContent` and `baseHref`.
 *
 * See `docs/superpowers/specs/2026-05-17-knit-output-webview-design.md`
 * for the threat model.
 */
export function buildShellHtml(args: {
    htmlContent: string;
    baseHref: string;
    cspSource: string;
    outputPath: string;
    nonce: string;
    /**
     * Absolute path of the source `.Rmd`. Persisted into the webview's
     * `setState` (alongside `outputPath`) so the `WebviewPanelSerializer`
     * can rebuild the panel after a window reload/restart: `outputPath`
     * locates the (old-session) rendered artifact to adopt, and
     * `sourceFsPath` is what "Knit again" and the current-session path
     * computation need. Omitted/empty in unit tests that don't exercise
     * persistence — `setState` is then skipped.
     */
    sourceFsPath?: string;
    /**
     * Effective knit `root.dir` for this preview, persisted into
     * `setState` alongside `sourceFsPath` so a serializer restore after
     * a window reload can resolve relative `include_graphics` images
     * against the same base the original knit used (rather than falling
     * back to the source directory, which is wrong in `project` mode or
     * when a chunk overrode `root.dir`). Omitted/empty when unknown.
     */
    knitRootDir?: string;
    /**
     * Persisted theme-toggle state. Caller reads it from
     * `context.globalState` so the choice survives panel disposal /
     * recreation between knits.
     */
    initialThemeApplied: boolean;
    /**
     * Pre-built CSS declarations for the active VS Code theme's
     * resolved palette, matching the same shape
     * `paletteCssDeclarations` returns. When non-null, the toggle's
     * applyTheme() prefers these colors over the GitHub variant. If
     * the resolver failed (theme JSON not found, parse error, etc.)
     * the caller passes `null` and the toggle falls back to the
     * GitHub variant — same behavior as before this feature shipped.
     */
    vscodeThemePaletteCss?: string | null;
    /**
     * Pre-built CSS declarations for the live-font override, matching
     * `fontsCssDeclarations`. When non-null, the webview applies these
     * fonts on first paint so the iframe never flashes the baked
     * (potentially-stale) fonts that are still in the on-disk `.html`.
     * The host re-pushes via `__ravenFontFamilies` on every
     * `onDidChangeConfiguration` event.
     */
    vscodeFontFamiliesCss?: string | null;
    /**
     * True when the workspace is remote (Remote SSH, Dev Containers,
     * WSL, Codespaces, etc. — `vscode.env.remoteName` is set). In a
     * remote workspace the **Open in Browser** action routes the
     * `file://` URI through the extension-host machine, so it cannot
     * reach the user's local browser. The toolbar button and the
     * matching right-click menu item are omitted from the rendered
     * shell entirely (via the `hidden` HTML attribute, which is
     * `display: none` per UA stylesheet and is also exposed as
     * `aria-hidden` to assistive tech). The DOM nodes still exist
     * so the bound JS handlers can attach without null checks; they
     * just never fire because hidden elements don't receive events.
     * Defaults to `false` so callers that don't supply it get the
     * local-workspace rendering.
     */
    isRemoteWorkspace?: boolean;
}): string {
    const {
        htmlContent,
        baseHref,
        cspSource,
        outputPath,
        nonce,
        sourceFsPath = '',
        knitRootDir = '',
        initialThemeApplied,
        vscodeThemePaletteCss,
        vscodeFontFamiliesCss,
        isRemoteWorkspace = false,
    } = args;
    // path.basename handles both POSIX and Windows separators.
    const lastSep = Math.max(outputPath.lastIndexOf('/'), outputPath.lastIndexOf('\\'));
    const basename = lastSep >= 0 ? outputPath.slice(lastSep + 1) : outputPath;
    const safeName = escapeHtml(basename);
    // Baked-in CSS strings for the two GitHub palette variants. The
    // overlay script picks one at runtime based on VS Code's current
    // theme variant (read from `document.body.className`) and writes
    // it into the iframe's `:root` so syntax-token colors stay in
    // sync with the live code-block background.
    const lightPaletteCss = paletteCssDeclarations(githubLight);
    const darkPaletteCss = paletteCssDeclarations(githubDark);

    // about:srcdoc bypasses `frame-src` per CSP3, but VS Code's webview
    // can occasionally route the inline document through a real URL
    // (e.g. when the iframe resolves a base-relative resource), so we
    // keep `frame-src ${cspSource}` to whitelist subresource frames as
    // well. `img-src`/`style-src`/`font-src` already permit
    // `${cspSource}` for the rendered HTML's assets.
    const csp = [
        `default-src 'none'`,
        `frame-src ${cspSource}`,
        `img-src ${cspSource} https: data:`,
        `style-src ${cspSource} 'unsafe-inline'`,
        `font-src ${cspSource} https: data:`,
        `script-src 'nonce-${nonce}'`,
        `connect-src 'none'`,
    ].join('; ');

    // Inject the base href so relative URLs in the rendered HTML
    // resolve through `webview.asWebviewUri(...)`, picking up the
    // outer webview's resource handler. Browsers honour a `<base>` tag
    // that appears anywhere in the head; HTML5 parsing creates an
    // implicit head when needed, so prepending is safe even for HTML
    // that already starts with `<!doctype html><html>...`.
    //
    // A `<base href>` also changes how *fragment-only* anchors
    // (`<a href="#section">`) are resolved: instead of resolving
    // against the document URL (`about:srcdoc`), they resolve against
    // the base URL, turning an in-document scroll into a full
    // navigation that fails for nested webview iframes. To preserve
    // intra-document anchor navigation, rewrite fragment-only hrefs
    // to be `about:srcdoc#…` — once the resolved URL matches the
    // document URL (sans fragment), the browser treats the click as
    // a same-document fragment navigation again.
    const srcdocHtml = `<base href="${escapeHtml(baseHref)}">`
        + rewriteFragmentAnchors(htmlContent);

    return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<title>Knit Preview</title>
<style nonce="${nonce}">
  body { margin: 0; padding: 0; height: 100vh; display: flex; flex-direction: column;
         font-family: var(--vscode-font-family); color: var(--vscode-foreground); }
  /*
   * Single-row, fixed-height toolbar modeled after the Plot Viewer's
   * .toolbar (editors/vscode/src/plot/webview/styles.css). All
   * buttons are icon-only so the row height never changes when a
   * label flips (Export -> Cancel) and the layout reads cleanly at
   * every panel width.
   *
   * flex-wrap: nowrap pins single-row behavior across future style
   * edits; overflow-x: auto plus a suppressed scrollbar is the
   * safety net for narrow panels where the intrinsic button widths
   * sum past the viewport — without it the implicit overflow on
   * body (iframe filling flex 1) would silently clip the right-most
   * buttons (Export, theme toggle).
   */
  #raven-knit-toolbar {
    display: flex; align-items: center; gap: 4px;
    padding: 4px 8px;
    background: var(--vscode-editorWidget-background);
    border-bottom: 1px solid var(--vscode-editorWidget-border);
    flex: 0 0 auto;
    flex-wrap: nowrap;
    overflow-x: auto;
    overflow-y: hidden;
    scrollbar-width: none;
  }
  #raven-knit-toolbar::-webkit-scrollbar { display: none; }
  #raven-knit-toolbar button {
    background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: 1px solid transparent;
    padding: 2px 6px;
    border-radius: 2px;
    font: inherit;
    cursor: pointer;
    flex-shrink: 0;
    white-space: nowrap;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 0;
  }
  #raven-knit-toolbar button:hover:not(:disabled) {
    background: var(--vscode-button-secondaryHoverBackground);
  }
  #raven-knit-toolbar button:disabled { opacity: 0.5; cursor: default; }
  /*
   * pointer-events: none on the inline SVG makes every click on
   * the icon fall through to the parent button, so event.target is
   * always the button itself. That gives a deterministic click
   * target for the popovertarget activation and any future event
   * handler that inspects event.target. Matches the plot viewer's
   * .toolbar .icon-btn svg rule.
   */
  #raven-knit-toolbar button svg {
    width: 16px; height: 16px; display: block; pointer-events: none;
  }
  #raven-knit-toolbar .raven-knit-spacer { flex: 1; }
  /*
   * Theme toggle "on" state. Mirrors the plot viewer toolbar's
   * .theme-toggle.is-on styling (App.svelte): the engaged state
   * fills the icon-only button with the same primary-button accent
   * VS Code uses for its main action buttons, so users immediately
   * read the toggle as active. The codicon's fill="currentColor"
   * picks up the matching button-foreground without an extra rule.
   *
   * The prior implementation used the inputOption-* CSS variables
   * (the Find widget's case-sensitive / whole-word treatment), but
   * those tokens are intentionally subtle — semi-transparent tints
   * over the input background that can disappear against the
   * editorWidget surface the toolbar sits on. The primary-button
   * palette here gives an unambiguous "pressed" colour that survives
   * every theme.
   */
  #raven-knit-toolbar button#raven-knit-theme[aria-pressed="true"] {
    background: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
  }
  #raven-knit-toolbar button#raven-knit-theme[aria-pressed="true"]:hover:not(:disabled) {
    background: var(--vscode-button-hoverBackground);
  }
  /*
   * Export busy state — when an export op is in flight, the icon
   * swaps from "export" to "stop-circle" via JS, and a subtle warning
   * tint signals "click again to cancel". The icon-swap means width
   * stays constant across busy/idle transitions; only the color
   * shifts.
   */
  #raven-knit-toolbar button#raven-knit-export[data-busy="true"] {
    color: var(--vscode-inputValidation-warningForeground,
                var(--vscode-foreground));
  }
  /*
   * Export popover. Mirrors the plot viewer's .share-popover (see
   * editors/vscode/src/plot/webview/styles.css) — same HTML popover
   * API for outside-click and Escape dismissal, same fixed
   * positioning via JS in positionExportPopover, same secondary-menu
   * surface variables.
   */
  #raven-knit-export-popover {
    position: fixed;
    inset: auto;
    margin: 0;
    padding: 4px;
    background: var(--vscode-editorWidget-background);
    color: var(--vscode-editorWidget-foreground, var(--vscode-foreground));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 4px;
    box-shadow: 0 2px 8px var(--vscode-widget-shadow, rgba(0, 0, 0, 0.36));
    min-width: 184px;
    display: none;
    flex-direction: column;
    gap: 2px;
    font-family: var(--vscode-font-family);
    font-size: 13px;
  }
  #raven-knit-export-popover:popover-open { display: flex; }
  #raven-knit-export-popover button {
    background: transparent;
    color: var(--vscode-foreground);
    border: 1px solid transparent;
    padding: 4px 12px;
    border-radius: 2px;
    font: inherit;
    cursor: pointer;
    text-align: left;
    white-space: nowrap;
  }
  #raven-knit-export-popover button:hover:not(:disabled),
  #raven-knit-export-popover button:focus-visible:not(:disabled) {
    background: var(--vscode-list-hoverBackground);
    outline: none;
  }
  #raven-knit-export-popover button:focus:not(:focus-visible) { outline: none; }
  #raven-knit-export-popover button:disabled { opacity: 0.5; cursor: default; }
  #raven-knit-frame { flex: 1 1 auto; width: 100%; border: 0; background: white; }
  #raven-knit-context-menu {
    position: fixed; min-width: 160px; z-index: 9999;
    padding: 0.25rem 0;
    background: var(--vscode-menu-background, var(--vscode-editorWidget-background));
    color: var(--vscode-menu-foreground, var(--vscode-foreground));
    border: 1px solid var(--vscode-menu-border, var(--vscode-editorWidget-border));
    box-shadow: 0 2px 8px var(--vscode-widget-shadow, rgba(0,0,0,0.3));
    font-family: var(--vscode-font-family); font-size: 13px;
  }
  #raven-knit-context-menu[hidden] { display: none; }
  #raven-knit-context-menu button {
    display: block; width: 100%; text-align: left;
    padding: 0.3rem 1rem;
    background: transparent; color: inherit; border: 0;
    font: inherit; cursor: pointer;
  }
  #raven-knit-context-menu button[hidden] { display: none; }
  /*
   * :focus-visible (not plain :focus) -- when the menu opens we
   * programmatically focus the first enabled item for accessibility,
   * but we do NOT want to paint a "this item is hovered" highlight
   * just because the focus moved there. :focus-visible activates
   * only when the focus came from a keyboard interaction (Tab, arrow
   * keys), which is the right time to show the selection ring.
   */
  #raven-knit-context-menu button:hover:not([disabled]),
  #raven-knit-context-menu button:focus-visible:not([disabled]) {
    background: var(--vscode-menu-selectionBackground,
                    var(--vscode-list-activeSelectionBackground));
    color: var(--vscode-menu-selectionForeground,
                var(--vscode-list-activeSelectionForeground));
    outline: none;
  }
  #raven-knit-context-menu button:focus:not(:focus-visible) {
    outline: none;
  }
  #raven-knit-context-menu button[disabled] { opacity: 0.5; cursor: default; }
</style>
</head>
<body>
  <div id="raven-knit-toolbar" role="toolbar" aria-label="Knit preview">
    <button id="raven-knit-refresh" type="button"
            aria-label="Knit again"
            title="Knit again (re-knit the source document)">${ICON_REFRESH}</button>
    <span class="raven-knit-spacer"></span>
    <!-- ARIA: the trigger carries \`aria-expanded\` so AT users hear
         "expanded / collapsed" on activation. \`popovertarget\` makes
         the browser handle the open/close toggle natively AND excludes
         the trigger from the popover's light-dismiss algorithm — without
         it, a click on the trigger while the popover is open would be
         classified as an outside-click, light-dismiss would close the
         popover on pointerup, and any JS click handler that then called
         showPopover() would just reopen it (i.e., the trigger could
         never close the popover). With \`popovertarget\` set, the
         browser also auto-mirrors aria-expanded for AT, and the
         \`beforetoggle\` listener below keeps our explicit attribute
         setting in lockstep for safety. \`aria-controls\` links the
         trigger to its popover for AT that supports popup tracking.
         We do NOT set \`aria-haspopup\` because the popover content is
         a labeled \`role="group"\` (not a \`role="menu"\` — no arrow-key
         navigation is implemented) and any specific aria-haspopup
         value would set incorrect expectations. Same reasoning as
         the plot viewer's share popover. -->
    <button id="raven-knit-export" type="button"
            aria-label="Export"
            aria-controls="raven-knit-export-popover"
            aria-expanded="false"
            popovertarget="raven-knit-export-popover"
            title="Export as HTML, PDF, or Word">${ICON_SHARE}</button>
    <button id="raven-knit-open-browser" type="button"${isRemoteWorkspace ? ' hidden' : ''}
            aria-label="Open in Browser"
            title="Open in Browser (open the rendered file in your default browser)">${ICON_GLOBE}</button>
    <button id="raven-knit-theme" type="button"
            aria-pressed="${initialThemeApplied ? 'true' : 'false'}"
            aria-label="Apply VS Code theme"
            title="Apply VS Code theme (recolor the rendered output to match the active editor theme)">${ICON_SYMBOL_COLOR}</button>
  </div>
  <!-- ARIA: \`role="group"\` rather than \`role="menu"\` because the HTML
       popover API gives Tab-order focus + Escape + outside-click
       dismissal but ARIA's menu pattern additionally requires
       arrow-key navigation (Up/Down/Home/End). We don't implement
       those handlers, so \`role="menu"\` would set incorrect AT
       expectations and WCAG-flag as incomplete keyboard interaction.
       Same reasoning as the plot viewer's share popover. -->
  <div id="raven-knit-export-popover"
       popover="auto"
       role="group"
       aria-label="Export ${safeName}">
    <button type="button" data-format="html">Export to HTML…</button>
    <button type="button" data-format="pdf">Export to PDF…</button>
    <button type="button" data-format="docx">Export to Word…</button>
  </div>
  <iframe id="raven-knit-frame"
          srcdoc="${escapeHtml(srcdocHtml)}"
          sandbox="allow-same-origin"
          title="Rendered output: ${safeName}"></iframe>
  <div id="raven-knit-context-menu" role="menu" hidden>
    <button type="button" role="menuitem" data-action="copy">Copy</button>
    <button type="button" role="menuitem" data-action="copy-image">Copy image</button>
    <button type="button" role="menuitem" data-action="select-all">Select All</button>
    <button type="button" role="menuitem" data-action="open-in-browser"${isRemoteWorkspace ? ' hidden' : ''}>Open in Browser</button>
  </div>
  <script nonce="${nonce}">
    (function () {
      const vscode = acquireVsCodeApi();
      // Persist enough for the WebviewPanelSerializer to rebuild this
      // panel after a window reload/restart: the source .Rmd path and
      // the rendered HTML path. VS Code returns this object to
      // \`deserializeWebviewPanel\` on the next launch. Empty
      // sourceFsPath means a non-persistence caller (e.g. a unit test)
      // built the shell — skip setState so we don't store a useless
      // record.
      var ravenRestoreState = {
        sourceFsPath: ${jsonForScript(sourceFsPath)},
        outputPath: ${jsonForScript(outputPath)},
        knitRootDir: ${jsonForScript(knitRootDir)},
      };
      if (ravenRestoreState.sourceFsPath) {
        try { vscode.setState(ravenRestoreState); } catch (e) { /* setState unavailable */ }
      }
      const iframe = document.getElementById('raven-knit-frame');
      const themeBtn = document.getElementById('raven-knit-theme');
      let loadFired = false;
      let errorFired = false;
      iframe.addEventListener('load', function () { loadFired = true; });
      iframe.addEventListener('error', function () { errorFired = true; });
      document.getElementById('raven-knit-refresh').addEventListener('click', function () {
        vscode.postMessage({ type: 'refresh' });
      });
      document.getElementById('raven-knit-open-browser').addEventListener('click', function () {
        vscode.postMessage({ type: 'openInBrowser' });
      });
      // Export button: idle state opens a webview-side popover with
      // format choices (HTML / PDF / Word); busy state cancels the
      // in-flight export. The format choice now crosses the trust
      // boundary via \`requestExport.format\` — \`isKnitOutputMessage\`
      // strictly validates against the \`EXPORT_FORMATS\` whitelist
      // before the host dispatches the matching \`raven.knit.export*\`
      // command. Icon swap (export -> stop-circle) and a single
      // unchanging tooltip per state keep the toolbar height invariant.
      const exportBtn = document.getElementById('raven-knit-export');
      const exportPopover = document.getElementById('raven-knit-export-popover');
      const ICON_EXPORT_SVG = ${JSON.stringify(ICON_SHARE)};
      const ICON_STOP_SVG = ${JSON.stringify(ICON_STOP)};
      // Capture the idle-state title/aria-label so syncExportBtn can
      // restore them when the busy->idle flip happens. The hardcoded
      // fallbacks are belt-and-braces: if a future markup edit drops
      // or empties either attribute, we still emit a non-empty value
      // on every state flip instead of silently stripping the
      // affordance after the first export completes.
      const exportTitleIdle = exportBtn.getAttribute('title') || 'Export as HTML, PDF, or Word';
      const exportAriaIdle = exportBtn.getAttribute('aria-label') || 'Export';
      const exportTitleBusy = 'Cancel the in-flight export';
      const exportAriaBusy = 'Cancel export';
      function syncExportBtn() {
        var busy = exportBtn.dataset.busy === 'true';
        exportBtn.innerHTML = busy ? ICON_STOP_SVG : ICON_EXPORT_SVG;
        exportBtn.setAttribute('title', busy ? exportTitleBusy : exportTitleIdle);
        exportBtn.setAttribute('aria-label', busy ? exportAriaBusy : exportAriaIdle);
      }
      function closeExportPopover() {
        // hidePopover() throws InvalidStateError when the popover is
        // not currently showing; guard with :popover-open before
        // calling so any caller (busy-state flip, format pick) is
        // safe to invoke at any time.
        if (exportPopover && exportPopover.matches && exportPopover.matches(':popover-open')) {
          if (exportPopover.hidePopover) exportPopover.hidePopover();
        }
      }
      // Position the popover under the export button. The HTML
      // popover API gives outside-click and Escape dismissal for
      // free; we only need to anchor the visible position. The
      // CSS rule \`#raven-knit-export-popover { inset: auto }\`
      // overrides the browser UA stylesheet's [popover] { inset: 0;
      // margin: auto } (which would otherwise center the popover)
      // so JS-set top/left actually take effect.
      //
      // The toolbar is at the top of the panel, so we always place
      // the popover BELOW the trigger (no flip-above needed). Both
      // axes clamp to keep the popover at least 4px from the viewport
      // edges so very narrow panels still render the menu legibly.
      function positionExportPopover() {
        if (!exportPopover || !exportBtn) return;
        var r = exportBtn.getBoundingClientRect();
        var vw = window.innerWidth;
        var vh = window.innerHeight;
        // If the trigger has scrolled out of the visible viewport
        // (e.g. the toolbar's overflow-x has clipped it on a very
        // narrow panel), there is no sensible anchor and the user
        // can't see what the menu belongs to. Close the popover
        // instead of clamping to a viewport corner with no visible
        // source button.
        var triggerVisible =
          r.right > 0 && r.left < vw && r.bottom > 0 && r.top < vh;
        if (!triggerVisible) {
          closeExportPopover();
          return;
        }
        // Clear stale inline coords before measuring so the popover
        // reports its natural box, not a previous position.
        exportPopover.style.left = '';
        exportPopover.style.top = '';
        exportPopover.style.right = '';
        exportPopover.style.bottom = '';
        // While :popover-open hasn't applied display: flex yet (we're
        // in beforetoggle), force display: flex for measurement and
        // hide via visibility so users don't see a flash at the UA
        // default centered position. Wrap in try/finally so an
        // unexpected throw during measurement can't leave the popover
        // permanently invisible via inline display/visibility styles.
        var prevDisplay = exportPopover.style.display;
        var prevVisibility = exportPopover.style.visibility;
        var w, h;
        try {
          exportPopover.style.visibility = 'hidden';
          exportPopover.style.display = 'flex';
          w = exportPopover.offsetWidth;
          h = exportPopover.offsetHeight;
        } finally {
          exportPopover.style.display = prevDisplay;
          exportPopover.style.visibility = prevVisibility;
        }

        // Anchor near the button's left edge by default; clamp so the
        // popover stays at least 4px from both viewport edges.
        var left = Math.max(4, Math.min(r.left, vw - w - 4));
        var top = r.bottom + 4;
        // If the toolbar is somehow not at the top (e.g. a future
        // layout change moves it), fall back to placing above when
        // the button is too close to the bottom of the viewport.
        if (top + h + 4 > vh && r.top - 4 - h >= 4) {
          top = r.top - 4 - h;
        }
        top = Math.max(4, top);
        exportPopover.style.left = left + 'px';
        exportPopover.style.top = top + 'px';
      }
      function onExportPopoverResize() {
        // Guard against the close-toggle race: if resize fires
        // between beforetoggle('closed') (sync) and toggle('closed')
        // (queued task that removes this listener), the popover is
        // mid-closing and the display: flex measurement trick would
        // briefly take effect on the closing element.
        if (!exportPopover || !exportPopover.matches || !exportPopover.matches(':popover-open')) return;
        positionExportPopover();
      }
      if (exportPopover) {
        exportPopover.addEventListener('beforetoggle', function (e) {
          // aria-expanded mirror MUST update synchronously with the
          // popover's visible state, not via the queued \`toggle\`
          // event — otherwise AT readers can briefly see the popover
          // open while the trigger still reads "collapsed". beforetoggle
          // fires sync; toggle fires as a queued task after the state
          // change has taken effect.
          exportBtn.setAttribute('aria-expanded', e.newState === 'open' ? 'true' : 'false');
          if (e.newState === 'open') {
            positionExportPopover();
          }
        });
        exportPopover.addEventListener('toggle', function (e) {
          if (e.newState === 'open') {
            var firstEnabled = exportPopover.querySelector('button:not([disabled])');
            if (firstEnabled) firstEnabled.focus();
            window.addEventListener('resize', onExportPopoverResize);
          } else {
            window.removeEventListener('resize', onExportPopoverResize);
          }
        });
        exportPopover.addEventListener('click', function (e) {
          var t = e.target;
          var btn = t && t.closest ? t.closest('button[data-format]') : null;
          if (!btn || btn.hasAttribute('disabled')) return;
          var format = btn.getAttribute('data-format');
          // Whitelist guard mirrors the host-side EXPORT_FORMATS check.
          // The host-side trust-boundary validator is the authoritative
          // check, but rejecting here prevents an unsupported format
          // from racing the popover-close and causing a stray
          // postMessage that would just bounce off the validator.
          if (format !== 'html' && format !== 'pdf' && format !== 'docx') return;
          closeExportPopover();
          vscode.postMessage({ type: 'requestExport', format: format });
        });
      }
      exportBtn.addEventListener('click', function (e) {
        if (exportBtn.dataset.busy === 'true') {
          // In busy mode the click is a cancel — preventDefault stops
          // the browser from processing the declarative popovertarget
          // invocation on the trigger, so we don't accidentally toggle
          // the popover while the user means to cancel.
          e.preventDefault();
          vscode.postMessage({ type: 'cancelExport' });
          return;
        }
        // Idle: the browser's declarative popovertarget="raven-knit-
        // export-popover" attribute on the trigger handles the toggle
        // (and crucially makes the trigger a popover invoker, so a
        // click on it is excluded from the light-dismiss algorithm —
        // see the markup-side comment above for why we don't rely on
        // an imperative showPopover/hidePopover here).
      });
      // Initial paint of the export icon. The button HTML already
      // ships with the export icon baked in via the template literal,
      // so this just keeps the syncExportBtn invariant on equal
      // footing with the busy-state postMessage path.
      syncExportBtn();

      // --- VS Code theme overlay -------------------------------------
      // The iframe is srcdoc + sandbox=allow-same-origin, which gives
      // the inner document the same origin as this outer shell. That
      // same-origin relationship is what lets the outer script inject
      // a style tag into the iframe contentDocument when the user
      // toggles the theme on. The injected stylesheet uses RESOLVED
      // color values from VS Code CSS variables (those variables are
      // defined on the outer shell html and do not propagate into the
      // iframe document), so when VS Code's active theme changes the
      // body class on the outer shell flips and we re-resolve + re-
      // inject.
      //
      // The initial value comes from the extension (which reads it
      // from globalState), embedded into the template literal below.
      // A toggle posts the new state back to the extension, which
      // persists it. We do not also call webview setState — every
      // shell render embeds the latest persisted value, and a hide/
      // show cycle leaves the in-memory variable intact.
      let themeApplied = ${initialThemeApplied ? 'true' : 'false'};

      // GitHub palette variants serialized at build time. We pick
      // one at overlay-apply time based on the active VS Code
      // theme variant so the syntax-token colors (which the
      // rendered document references via --raven-c-*) match the
      // live code-block background. Without this swap, switching
      // themes after knit could leave e.g. dark-palette tokens on
      // a light textCodeBlock background.
      const RAVEN_PALETTE_CSS = {
        light: ${JSON.stringify(lightPaletteCss)},
        dark: ${JSON.stringify(darkPaletteCss)},
      };

      // Resolved VS Code theme palette. When non-null, the toggle
      // paints these colors instead of the GitHub variant. The
      // extension host posts replacement values whenever the active
      // theme or relevant editor.* settings change; the receiver
      // below applies the update and re-runs applyTheme so the
      // change shows up immediately without a re-knit.
      let vscodePaletteCss = ${
          typeof vscodeThemePaletteCss === 'string' && vscodeThemePaletteCss.length > 0
              ? JSON.stringify(vscodeThemePaletteCss)
              : 'null'
      };

      // Live font override. The on-disk .html has its own
      // \`--raven-font-text\` / \`--raven-font-mono\` baked into the
      // iframe's :root; this script appends a SECOND :root rule in a
      // dedicated <style id="raven-vscode-font-overrides"> element so
      // the live value wins on the cascade (last-equal-specificity
      // wins). Re-applies on iframe load + on every
      // \`__ravenFontFamilies\` postMessage so the setting takes effect
      // without a re-knit.
      let vscodeFontCss = ${
          typeof vscodeFontFamiliesCss === 'string' && vscodeFontFamiliesCss.length > 0
              ? JSON.stringify(vscodeFontFamiliesCss)
              : 'null'
      };

      function activePaletteVariant() {
        // Mirror the regex used by render-html.ts:composeStylesheet
        // so the overlay-time variant choice matches the bake-time
        // logic. vscode-high-contrast (no -light suffix) is the
        // dark high-contrast variant; vscode-high-contrast-light
        // is the light one.
        const cls = document.body.className || '';
        return /\\bvscode-(light|high-contrast-light)\\b/.test(cls) ? 'light' : 'dark';
      }

      function activePaletteCss() {
        // VS Code-extracted palette wins over the GitHub default.
        // Falling through to GitHub when the resolver failed lets
        // the toggle still produce a coherent code-block look
        // matching the live background.
        if (vscodePaletteCss !== null) return vscodePaletteCss;
        return RAVEN_PALETTE_CSS[activePaletteVariant()];
      }

      function syncThemeBtn() {
        // Rmd output has no "document theme" — the toggle just
        // controls whether VS Code theming is overlaid. Keep the
        // button label constant; the active state is conveyed
        // visually via aria-pressed (which CSS styles).
        themeBtn.setAttribute('aria-pressed', themeApplied ? 'true' : 'false');
      }

      function readThemeColors() {
        const cs = getComputedStyle(document.documentElement);
        function v(name, fallback) {
          const x = cs.getPropertyValue(name).trim();
          return x.length > 0 ? x : fallback;
        }
        const bg = v('--vscode-editor-background', '#1e1e1e');
        return {
          bg: bg,
          fg: v('--vscode-editor-foreground', '#cccccc'),
          link: v('--vscode-textLink-foreground', '#3794ff'),
          // textCodeBlock-background is the variable VS Code's own
          // markdown preview uses for code-block shading; it's
          // defined by most themes with a subtle tint relative to
          // the editor background. Fall back to editor-background
          // for themes that don't set it so the block bg at least
          // matches the surrounding surface.
          codeBg: v('--vscode-textCodeBlock-background', bg),
        };
      }

      // Local SVG plots are loaded as data: URL <img> elements first:
      // image-loaded SVG is inert (scripts do not run) and preserves
      // the nested-iframe image-loading workaround. Before applying
      // the VS Code theme overlay, we replace only images explicitly
      // marked by the extension host as knitr figure SVGs with
      // sanitized inline SVG nodes. Inline SVG is required because CSS
      // cannot reach inside an <img>-loaded SVG document.
      const RAVEN_SVG_SAFE_STYLE_PROPS = {
        'fill': true,
        'fill-opacity': true,
        'fill-rule': true,
        'stroke': true,
        'stroke-width': true,
        'stroke-linecap': true,
        'stroke-linejoin': true,
        'stroke-dasharray': true,
        'stroke-dashoffset': true,
        'stroke-miterlimit': true,
        'stroke-opacity': true,
        'opacity': true,
        'font-size': true,
        'font-family': true,
        'font-style': true,
        'font-weight': true,
        'font-variant': true,
        'text-anchor': true,
        'dominant-baseline': true,
        'alignment-baseline': true,
        'color': true,
        'visibility': true,
      };
      const RAVEN_SVG_FORBID_TAGS = {
        'script': true,
        'style': true,
        'foreignobject': true,
        'image': true,
        'a': true,
        'iframe': true,
        'object': true,
        'embed': true,
        'feimage': true,
      };
      function decodeSvgDataUrl(src) {
        if (typeof src !== 'string') return null;
        var comma = src.indexOf(',');
        if (comma < 0) return null;
        var meta = src.slice(0, comma);
        if (!/^data:image\\/svg\\+xml(?:[;,]|$)/i.test(meta)) return null;
        var data = src.slice(comma + 1);
        var suffixAt = data.search(/[?#]/);
        if (suffixAt >= 0) data = data.slice(0, suffixAt);
        try {
          if (/;base64(?:[;,]|$)/i.test(meta)) {
            var binary = atob(data);
            if (typeof TextDecoder !== 'undefined') {
              var bytes = new Uint8Array(binary.length);
              for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
              return new TextDecoder('utf-8').decode(bytes);
            }
            return decodeURIComponent(Array.prototype.map.call(binary, function (ch) {
              return '%' + ('00' + ch.charCodeAt(0).toString(16)).slice(-2);
            }).join(''));
          }
          return decodeURIComponent(data);
        } catch (e) {
          return null;
        }
      }
      function svgStyleValueIsSafe(value) {
        return !/url\\s*\\(|expression\\s*\\(|javascript:|@|[<>]/i.test(value);
      }
      function migrateInlineSvgStyles(root) {
        var nodes = [root].concat(Array.prototype.slice.call(root.querySelectorAll('*')));
        for (var i = 0; i < nodes.length; i++) {
          var el = nodes[i];
          var style = el.getAttribute('style');
          if (!style) continue;
          var declarations = style.split(';');
          for (var j = 0; j < declarations.length; j++) {
            var decl = declarations[j];
            var colon = decl.indexOf(':');
            if (colon < 0) continue;
            var prop = decl.slice(0, colon).trim().toLowerCase();
            var value = decl.slice(colon + 1).trim();
            if (!prop || !value) continue;
            if (!RAVEN_SVG_SAFE_STYLE_PROPS[prop]) continue;
            if (!svgStyleValueIsSafe(value)) continue;
            if (!el.hasAttribute(prop)) el.setAttribute(prop, value);
          }
          el.removeAttribute('style');
        }
      }
      function valueHasUnsafeSvgUrl(value) {
        if (!/url\\s*\\(/i.test(value)) return false;
        var re = /url\\(\\s*(['"]?)([^'")]+)\\1\\s*\\)/gi;
        var saw = false;
        var m;
        while ((m = re.exec(value)) !== null) {
          saw = true;
          if (m[2].trim().charAt(0) !== '#') return true;
        }
        return !saw;
      }
      function hrefIsSafeInternalUse(el, name, value) {
        if (el.localName.toLowerCase() !== 'use') return false;
        if (name !== 'href' && name !== 'xlink:href') return false;
        return /^\\s*#[-A-Za-z0-9_:.]+\\s*$/.test(value);
      }
      function sanitizeKnitPlotSvg(svgText, doc) {
        var parsed;
        try {
          parsed = new DOMParser().parseFromString(svgText, 'image/svg+xml');
        } catch (e) {
          return null;
        }
        var parsedRoot = parsed && parsed.documentElement;
        if (!parsedRoot || parsedRoot.localName.toLowerCase() !== 'svg') return null;
        var svg = doc.importNode(parsedRoot, true);
        migrateInlineSvgStyles(svg);
        var nodes = [svg].concat(Array.prototype.slice.call(svg.querySelectorAll('*')));
        for (var i = nodes.length - 1; i >= 0; i--) {
          var el = nodes[i];
          if (RAVEN_SVG_FORBID_TAGS[el.localName.toLowerCase()]) {
            if (el.parentNode) el.parentNode.removeChild(el);
            continue;
          }
          var attrs = Array.prototype.slice.call(el.attributes || []);
          for (var j = 0; j < attrs.length; j++) {
            var attr = attrs[j];
            var name = attr.name.toLowerCase();
            var value = attr.value || '';
            if (name.indexOf('on') === 0) {
              el.removeAttribute(attr.name);
              continue;
            }
            if (name === 'style') {
              el.removeAttribute(attr.name);
              continue;
            }
            if (name === 'href' || name === 'xlink:href') {
              if (!hrefIsSafeInternalUse(el, name, value)) {
                el.removeAttribute(attr.name);
              }
              continue;
            }
            if (/javascript:/i.test(value) || valueHasUnsafeSvgUrl(value)) {
              el.removeAttribute(attr.name);
            }
          }
        }
        svg.classList.add('raven-knit-plot-svg');
        tagKnitPlotGlyphPaths(svg);
        tagKnitPlotBackgroundRects(svg);
        tagKnitPlotBackgroundPaths(svg);
        return svg;
      }
      function firstRectChild(parent) {
        for (var n = parent.firstElementChild; n !== null; n = n.nextElementSibling) {
          if (n.localName.toLowerCase() === 'rect') return n;
        }
        return null;
      }
      function isInitialDirectSvgRect(rect) {
        var parent = rect.parentElement;
        if (!parent || parent.localName.toLowerCase() !== 'svg') return false;
        var sawRect = false;
        for (var n = parent.firstElementChild; n !== null; n = n.nextElementSibling) {
          var name = n.localName.toLowerCase();
          if (name === 'defs' && !sawRect) continue;
          if (n === rect) return true;
          if (name === 'rect') {
            sawRect = true;
            continue;
          }
          if (name !== 'rect') return false;
        }
        return false;
      }
      function isKnitPlotBackgroundRect(rect) {
        var parent = rect.parentElement;
        if (!parent) return false;
        var parentName = parent.localName.toLowerCase();
        if (parentName === 'svg') {
          if (firstRectChild(parent) === rect) return true;
          // grDevices::svg commonly emits duplicate top-level canvas
          // rects before any plot geometry. Treat only that initial run
          // as background; later direct rects remain content.
          return isInitialDirectSvgRect(rect);
        }
        if (parentName === 'g') {
          if (rect.hasAttribute('stroke-linejoin')) return false;
          if (rect.hasAttribute('stroke-linecap')) return false;
          return true;
        }
        return false;
      }
      function tagKnitPlotBackgroundRects(svg) {
        var rects = svg.querySelectorAll('rect');
        for (var i = 0; i < rects.length; i++) {
          if (isKnitPlotBackgroundRect(rects[i])) {
            rects[i].classList.add('raven-bg');
          }
        }
      }
      function tagKnitPlotGlyphPaths(svg) {
        var paths = svg.querySelectorAll('defs path');
        for (var i = 0; i < paths.length; i++) {
          paths[i].classList.add('raven-text-glyph');
        }
      }
      function firstClipPathGroup(svg) {
        for (var n = svg.firstElementChild; n !== null; n = n.nextElementSibling) {
          if (n.localName.toLowerCase() === 'g' && n.hasAttribute('clip-path')) return n;
        }
        return null;
      }
      function pathBounds(d) {
        var matches = String(d || '').match(/[-+]?(?:\\d+\\.?\\d*|\\.\\d+)(?:[eE][-+]?\\d+)?/g);
        if (!matches || matches.length < 8 || matches.length % 2 !== 0) return null;
        var minX = Infinity;
        var minY = Infinity;
        var maxX = -Infinity;
        var maxY = -Infinity;
        for (var i = 0; i < matches.length; i += 2) {
          var x = Number(matches[i]);
          var y = Number(matches[i + 1]);
          if (!Number.isFinite(x) || !Number.isFinite(y)) return null;
          minX = Math.min(minX, x);
          minY = Math.min(minY, y);
          maxX = Math.max(maxX, x);
          maxY = Math.max(maxY, y);
        }
        return { minX: minX, minY: minY, maxX: maxX, maxY: maxY };
      }
      function boundsNearlyEqual(a, b) {
        if (!a || !b) return false;
        var epsilon = 0.1;
        return Math.abs(a.minX - b.minX) <= epsilon
          && Math.abs(a.minY - b.minY) <= epsilon
          && Math.abs(a.maxX - b.maxX) <= epsilon
          && Math.abs(a.maxY - b.maxY) <= epsilon;
      }
      function clipPathBoundsForGroup(group, svg) {
        var clip = (group.getAttribute('clip-path') || '').trim();
        var m = /^url\\(#([-A-Za-z0-9_:.]+)\\)$/.exec(clip);
        if (!m) return null;
        var clips = svg.querySelectorAll('clipPath');
        for (var i = 0; i < clips.length; i++) {
          if (clips[i].getAttribute('id') !== m[1]) continue;
          var pathEl = clips[i].querySelector('path');
          return pathEl ? pathBounds(pathEl.getAttribute('d') || '') : null;
        }
        return null;
      }
      function isKnitPlotBackgroundPath(pathEl, firstClippedGroup, svg) {
        var parent = pathEl.parentElement;
        if (!parent || parent !== firstClippedGroup) return false;
        var stroke = (pathEl.getAttribute('stroke') || '').trim().toLowerCase();
        if (stroke.length > 0 && stroke !== 'none') return false;
        var fill = (pathEl.getAttribute('fill') || '').trim().toLowerCase();
        if (fill.length === 0 || fill === 'none') return false;
        // ggplot backgrounds emitted by grDevices::svg are filled paths
        // in the first clipped group. Only hide that path when its bounds
        // match the clipping rectangle; an early filled data layer should
        // remain visible even if it also has no stroke.
        return boundsNearlyEqual(
          pathBounds(pathEl.getAttribute('d') || ''),
          clipPathBoundsForGroup(firstClippedGroup, svg),
        );
      }
      function tagKnitPlotBackgroundPaths(svg) {
        var firstClippedGroup = firstClipPathGroup(svg);
        if (!firstClippedGroup) return;
        var paths = svg.querySelectorAll('path');
        for (var i = 0; i < paths.length; i++) {
          var pathEl = paths[i];
          if (isKnitPlotBackgroundPath(pathEl, firstClippedGroup, svg)) {
            pathEl.classList.add('raven-bg');
          }
        }
      }
      function inlineKnitSvgPlots(doc) {
        var imgs = doc.querySelectorAll('img[data-raven-plot-svg="true"]');
        for (var i = 0; i < imgs.length; i++) {
          var img = imgs[i];
          var svgText = decodeSvgDataUrl(img.getAttribute('src') || '');
          if (!svgText) continue;
          var svg = sanitizeKnitPlotSvg(svgText, doc);
          if (!svg) continue;
          var host = doc.createElement('span');
          host.className = 'raven-knit-plot-host';
          var alt = img.getAttribute('alt') || '';
          if (alt.length > 0) {
            host.setAttribute('role', 'img');
            host.setAttribute('aria-label', alt);
          }
          host.appendChild(svg);
          img.replaceWith(host);
        }
      }

      function applyTheme() {
        const doc = iframe.contentDocument;
        if (!doc || !doc.documentElement) return;
        // contentDocument's head exists on parsed HTML; for srcdoc
        // iframes we may race the parser, so fall back to <html>.
        const host = doc.head || doc.documentElement;
        let style = doc.getElementById('raven-vscode-theme-overrides');
        if (!themeApplied) {
          if (style) style.remove();
          iframe.style.background = '';
          syncThemeBtn();
          return;
        }
        inlineKnitSvgPlots(doc);
        if (!style) {
          style = doc.createElement('style');
          style.id = 'raven-vscode-theme-overrides';
          host.appendChild(style);
        }
        const c = readThemeColors();
        // The GitHub-palette base stylesheet paints both <pre> and
        // its inner <code> with --raven-bg. Override both so the
        // syntax-highlight wrapper and any inline <code> in prose
        // pick up the theme's code-block shading. We ALSO re-emit
        // the matching GitHub palette variant on :root: token spans
        // reference --raven-c-* via var(), so updating those vars
        // here cascades into them automatically and keeps the
        // syntax-token foreground in lockstep with the live code-
        // block background. Without this, a VS Code theme switch
        // after knit would update --vscode-textCodeBlock-background
        // (resolved live from the outer shell) while leaving token
        // colors on the baked-at-knit variant — i.e. dark tokens
        // on a light background, or vice versa.
        const variantCss = activePaletteCss();
        style.textContent =
          ':root { ' + variantCss + ' }'
          + ' html, body { background: ' + c.bg + ' !important; '
          + 'color: ' + c.fg + ' !important; }'
          + ' a { color: ' + c.link + ' !important; }'
          // Block code: paint c.codeBg on pre.raven-knit-code only
          // (input chunks). textCodeBlock-background is often a
          // semi-transparent overlay (VS Code default for dark themes
          // is rgba(10,10,10,0.4)); applying it to BOTH pre AND its
          // descendant code stacks the overlay twice inside the code
          // text area, producing a visible highlight around the text
          // vs the pre padding region. The pre code rule below forces
          // ANY code descendant of any pre (including span-wrapped
          // pre>span>code shapes that some plugins emit) to
          // transparent, so it shows through to whatever its
          // surrounding pre paints. Output blocks (untagged pre
          // elements) are explicitly flattened so they read as
          // prose-with-monospace, the way Quarto's preview surfaces
          // output. The flatten rule deliberately omits !important so
          // user-authored pre style="..." in asis output keeps its
          // inline style.
          + ' pre.raven-knit-code { background: ' + c.codeBg + ' !important; }'
          + ' pre code { background: transparent !important; }'
          + ' pre:not(.raven-knit-code) {'
          + ' background: transparent; border: 0; padding: 0; }'
          // Inline code in prose should pick up the textCodeBlock
          // shading so the inline form matches the block form's
          // surface. We paint ALL code with c.codeBg by default and
          // rely on the pre code rule above to override for code
          // inside any pre — including span-wrapped or otherwise
          // nested code that a strict child combinator would miss.
          + ' code { background: ' + c.codeBg + ' !important; }'
          // Defensive: zero out every paint property that could
          // give code-block spans a per-token visual chrome. Spans
          // are inline elements whose background-color should never
          // paint, but some webview rendering paths apply subtle
          // effects to highlighted text. Forcing the relevant
          // properties to no-paint defaults keeps the rendered code
          // looking like the editor.
          + ' pre code span, code span {'
          + ' background: transparent !important;'
          + ' background-color: transparent !important;'
          + ' background-image: none !important;'
          + ' text-shadow: none !important;'
          + ' box-shadow: none !important;'
          + ' outline: none !important;'
          + ' border: 0 !important;'
          + ' filter: none !important;'
          + ' text-decoration: none !important; }'
          // SVG plots: after panel-side sanitization/inlining, the
          // overlay can reach into the plot the same way the plot
          // viewer's inline-SVG substrate does. The use selector covers R's
          // grDevices SVG text glyphs, which are emitted as internal
          // <use href="#glyph-..."> references rather than <text>.
          + ' .raven-knit-plot-host { background: ' + c.bg + ' !important; }'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg text,'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg use {'
          + ' fill: ' + c.fg + ' !important;'
          + ' font-family: var(--raven-font-text) !important; }'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg line,'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg polyline,'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg polygon,'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg path:not(.raven-bg):not(.raven-text-glyph),'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg circle,'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg rect:not(.raven-bg) {'
          + ' stroke: ' + c.fg + ' !important; }'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg path.raven-text-glyph {'
          + ' stroke: none !important; }'
          + ' .raven-knit-plot-host svg.raven-knit-plot-svg .raven-bg {'
          + ' fill: none !important; stroke: none !important; }';
        // Paint the iframe element itself too so the brief flash
        // before the inner document parses also matches the theme.
        iframe.style.background = c.bg;
        syncThemeBtn();
      }

      themeBtn.addEventListener('click', function () {
        themeApplied = !themeApplied;
        // Tell the extension so it can persist the choice in
        // globalState; the next panel render reads the saved value
        // back via initialThemeApplied.
        vscode.postMessage({ type: 'themeChanged', applied: themeApplied });
        applyTheme();
      });

      iframe.addEventListener('load', applyTheme);
      // The srcdoc parse may have completed before our script ran;
      // try immediately in that case.
      if (iframe.contentDocument
          && iframe.contentDocument.readyState !== 'loading') {
        applyTheme();
      }

      // Live-font override is independent of the VS Code-theme toggle:
      // fonts always track the user setting (no UX dial), while colors
      // require explicit opt-in via themeBtn. Apply on every iframe
      // (re)load so a panel.webview.html swap re-injects the override
      // into the fresh document.
      function applyFonts() {
        const doc = iframe.contentDocument;
        if (!doc || !doc.documentElement) return;
        const host = doc.head || doc.documentElement;
        let style = doc.getElementById('raven-vscode-font-overrides');
        if (vscodeFontCss === null) {
          if (style) style.remove();
          return;
        }
        if (!style) {
          style = doc.createElement('style');
          style.id = 'raven-vscode-font-overrides';
          host.appendChild(style);
        }
        // CSS specificity for two equal-specificity :root selectors:
        // last-wins. Our override is appended AFTER the baked
        // declarations, so this rule wins for both --raven-font-text
        // and --raven-font-mono.
        style.textContent = ':root { ' + vscodeFontCss + ' }';
      }
      iframe.addEventListener('load', applyFonts);
      if (iframe.contentDocument
          && iframe.contentDocument.readyState !== 'loading') {
        applyFonts();
      }

      // --- Copy / Select All / context menu ------------------------
      // VS Code disables the browser's default context menu inside
      // webviews and does not forward Cmd/Ctrl-C to the host clipboard
      // command when the keyboard focus is in a nested iframe. Since
      // the iframe is same-origin (sandbox=allow-same-origin + srcdoc
      // gives it the parent webview's origin), the outer shell can
      // attach handlers to iframe.contentWindow directly and reach
      // the selection.
      const ctxMenu = document.getElementById('raven-knit-context-menu');
      const ctxCopyBtn = ctxMenu.querySelector('[data-action="copy"]');
      const ctxCopyImageBtn = ctxMenu.querySelector('[data-action="copy-image"]');
      // The image-like element the user right-clicked, captured at
      // contextmenu time. This is usually an <img>, but knitr SVG
      // plots become inline <svg> nodes so the theme overlay can
      // reach them. Cleared when the menu hides so a stale reference
      // can't leak into a follow-up Copy from a text selection.
      // We capture the element (not just its src) because the
      // canvas-based copy below reads pixels from already-loaded
      // content — fetch() is blocked by the outer-shell CSP's
      // connect-src 'none', so going back to the network would fail
      // for every supported source kind.
      let pendingImage = null;

      function copyIframeSelection() {
        const win = iframe.contentWindow;
        if (!win) return false;
        const sel = win.getSelection();
        const text = sel ? sel.toString() : '';
        if (!text) return false;
        // Prefer the async Clipboard API; fall back to execCommand
        // for older webviews. The keypress / contextmenu-click that
        // triggers this counts as a user gesture, satisfying both
        // browser permission models.
        try {
          if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(text);
            return true;
          }
        } catch (e) { /* fall through */ }
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'absolute';
        ta.style.left = '-9999px';
        document.body.appendChild(ta);
        ta.select();
        let ok = false;
        try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
        document.body.removeChild(ta);
        return ok;
      }

      function selectAllInIframe() {
        const doc = iframe.contentDocument;
        const win = iframe.contentWindow;
        if (!doc || !win || !doc.body) return;
        const range = doc.createRange();
        range.selectNodeContents(doc.body);
        const sel = win.getSelection();
        if (sel) {
          sel.removeAllRanges();
          sel.addRange(range);
        }
      }

      // Copy the right-clicked image onto the system clipboard.
      // Draws the already-loaded image onto an offscreen canvas
      // and writes the canvas as a PNG blob via the async
      // Clipboard API. We use canvas rather than fetch because the
      // outer-shell CSP sets connect-src 'none', which blocks any
      // JS-initiated request (including same-origin local-resource
      // URLs). The image element has already loaded its pixels by
      // the time the user right-clicks, so the canvas approach
      // needs no further network access. Output is always PNG so
      // the clipboard MIME type is deterministic and supported on
      // every platform.
      function copyImageFromIframe() {
        const img = pendingImage;
        if (!img) return;
        const w = window;
        const hasClipboardImage = !!(w.ClipboardItem
          && navigator.clipboard && navigator.clipboard.write);
        // Fall back to copying the URL when ClipboardItem is
        // unavailable or when the image is cross-origin (canvas
        // would taint and toBlob would throw). The user can still
        // paste the URL into another tool to pick the image up.
        function copyUrlFallback() {
          if (navigator.clipboard && navigator.clipboard.writeText) {
            if (img.src) {
              navigator.clipboard.writeText(img.src).catch(function () { /* swallow */ });
              return;
            }
            var fallback = '';
            if (!fallback && String(img.tagName).toLowerCase() === 'svg') {
              try { fallback = new XMLSerializer().serializeToString(img); }
              catch (e) { fallback = ''; }
            }
            if (fallback) navigator.clipboard.writeText(fallback).catch(function () { /* swallow */ });
          }
        }
        if (!hasClipboardImage) { copyUrlFallback(); return; }
        if (String(img.tagName).toLowerCase() === 'svg') {
          copyInlineSvgImage(img, copyUrlFallback);
          return;
        }
        try {
          const canvas = document.createElement('canvas');
          canvas.width = img.naturalWidth || img.width;
          canvas.height = img.naturalHeight || img.height;
          if (canvas.width === 0 || canvas.height === 0) { copyUrlFallback(); return; }
          const ctx = canvas.getContext('2d');
          if (!ctx) { copyUrlFallback(); return; }
          ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
          // canvas.toBlob throws SecurityError (or yields null
          // depending on platform) when the canvas is tainted by a
          // cross-origin image without CORS headers.
          canvas.toBlob(function (blob) {
            if (!blob) { copyUrlFallback(); return; }
            try {
              const item = new w.ClipboardItem({ 'image/png': blob });
              navigator.clipboard.write([item]).catch(copyUrlFallback);
            } catch (e) { copyUrlFallback(); }
          }, 'image/png');
        } catch (e) { copyUrlFallback(); }
      }

      function copyInlineSvgImage(svg, copyUrlFallback) {
        try {
          const rect = svg.getBoundingClientRect();
          var width = Math.round(rect.width || 0);
          var height = Math.round(rect.height || 0);
          if ((!width || !height) && svg.viewBox && svg.viewBox.baseVal) {
            width = Math.round(svg.viewBox.baseVal.width || width);
            height = Math.round(svg.viewBox.baseVal.height || height);
          }
          if (!width) width = Math.round(parseFloat(svg.getAttribute('width') || '0'));
          if (!height) height = Math.round(parseFloat(svg.getAttribute('height') || '0'));
          if (!width || !height) { copyUrlFallback(); return; }
          const source = new XMLSerializer().serializeToString(svg);
          const image = new Image();
          image.onload = function () {
            try {
              const canvas = document.createElement('canvas');
              canvas.width = width;
              canvas.height = height;
              const ctx = canvas.getContext('2d');
              if (!ctx) { copyUrlFallback(); return; }
              ctx.drawImage(image, 0, 0, width, height);
              canvas.toBlob(function (blob) {
                if (!blob) { copyUrlFallback(); return; }
                try {
                  const item = new window.ClipboardItem({ 'image/png': blob });
                  navigator.clipboard.write([item]).catch(copyUrlFallback);
                } catch (e) { copyUrlFallback(); }
              }, 'image/png');
            } catch (e) { copyUrlFallback(); }
          };
          image.onerror = copyUrlFallback;
          image.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(source);
        } catch (e) { copyUrlFallback(); }
      }

      function hideContextMenu() {
        ctxMenu.hidden = true;
        pendingImage = null;
      }

      function showContextMenu(clientX, clientY, hasSelection, image) {
        if (hasSelection) {
          ctxCopyBtn.removeAttribute('disabled');
        } else {
          ctxCopyBtn.setAttribute('disabled', 'true');
        }
        if (image) {
          ctxCopyImageBtn.removeAttribute('disabled');
          pendingImage = image;
        } else {
          ctxCopyImageBtn.setAttribute('disabled', 'true');
          pendingImage = null;
        }
        // Render off-screen first to measure, then clamp into the
        // viewport so the menu never spills past the right/bottom
        // edge of the webview.
        ctxMenu.style.left = '-9999px';
        ctxMenu.style.top = '0';
        ctxMenu.hidden = false;
        const r = ctxMenu.getBoundingClientRect();
        const vw = window.innerWidth, vh = window.innerHeight;
        const x = Math.max(0, Math.min(clientX, vw - r.width - 2));
        const y = Math.max(0, Math.min(clientY, vh - r.height - 2));
        ctxMenu.style.left = x + 'px';
        ctxMenu.style.top = y + 'px';
        const firstEnabled = ctxMenu.querySelector('button:not([disabled])');
        if (firstEnabled) firstEnabled.focus();
      }

      ctxMenu.addEventListener('click', function (e) {
        const btn = e.target.closest
          ? e.target.closest('button[data-action]')
          : null;
        if (!btn || btn.hasAttribute('disabled')) return;
        const action = btn.getAttribute('data-action');
        if (action === 'copy') copyIframeSelection();
        else if (action === 'copy-image') copyImageFromIframe();
        else if (action === 'select-all') selectAllInIframe();
        else if (action === 'open-in-browser') vscode.postMessage({ type: 'openInBrowser' });
        hideContextMenu();
      });

      // Outer-shell dismiss handlers — click anywhere outside the
      // menu, scroll the toolbar, or hit Escape.
      document.addEventListener('mousedown', function (e) {
        if (ctxMenu.hidden) return;
        if (!ctxMenu.contains(e.target)) hideContextMenu();
      });
      document.addEventListener('keydown', function (e) {
        // Layered unwind to match the iframe Escape handler: context
        // menu first (topmost layer when summoned over an open
        // popover via right-click), then the export popover. The
        // browser's native popover Escape would dismiss the popover
        // on its own, but routing it through closeExportPopover()
        // here makes sure a single Escape press only collapses one
        // layer even when both are open with outer-shell focus.
        if (e.key !== 'Escape') return;
        if (!ctxMenu.hidden) {
          hideContextMenu();
          e.preventDefault();
          return;
        }
        if (exportPopover
            && exportPopover.matches
            && exportPopover.matches(':popover-open')) {
          closeExportPopover();
          e.preventDefault();
          return;
        }
      });

      function attachIframeInputHandlers() {
        const win = iframe.contentWindow;
        const doc = iframe.contentDocument;
        if (!win || !doc) return;
        // Cmd/Ctrl-C and Cmd/Ctrl-A while the iframe has focus.
        // For every other *modifier* shortcut we synthesize the
        // keydown event on the outer shell document so VS Code's
        // keybinding handler sees it. The iframe is sandboxed and
        // keystrokes that fire inside it don't reach VS Code's
        // chrome otherwise; the same-origin sandbox lets us reach
        // across the document boundary to re-dispatch.
        //
        // We gate on the modifier so plain typing inside any
        // input/widget rendered in the report does NOT bubble out
        // and silently trigger a single-key keybinding the user
        // may have configured in VS Code.
        win.addEventListener('keydown', function (e) {
          // Escape dismisses any open toolbar UI even when focus is
          // inside the iframe. Keystrokes that fire inside a sandboxed
          // (allow-same-origin) iframe stay within its document tree,
          // so the HTML popover API's built-in Escape light-dismiss
          // (which listens on the OUTER shell document) never sees
          // them — without this branch a user who clicks into the
          // rendered report and presses Escape would be stuck with
          // an open Export menu they cannot close from the keyboard.
          // Mirrors the existing mousedown -> dismissToolbarUi route
          // that closes the popover on iframe clicks.
          if (e.key === 'Escape') {
            // Unwind one layer per Escape press — the context menu
            // (when summoned over an open popover via right-click)
            // is the topmost layer and dismisses first. The popover
            // remains; a second Escape closes it. Matches the
            // conventional "Escape pops one modal" affordance.
            if (!ctxMenu.hidden) {
              hideContextMenu();
              e.preventDefault();
              return;
            }
            if (exportPopover
                && exportPopover.matches
                && exportPopover.matches(':popover-open')) {
              closeExportPopover();
              e.preventDefault();
              return;
            }
          }
          const mod = e.metaKey || e.ctrlKey;
          if (!mod) return;
          // AltGr on Windows / many Linux layouts fires as Ctrl+Alt
          // for typing characters like @, €, or accented letters.
          // The platform reports the AltGraph modifier state on
          // those keypresses; skip them so the user can type.
          if (e.getModifierState && e.getModifierState('AltGraph')) return;
          const k = (e.key || '').toLowerCase();
          if (k === 'c') {
            if (copyIframeSelection()) e.preventDefault();
            return;
          }
          if (k === 'a') {
            selectAllInIframe();
            e.preventDefault();
            return;
          }
          // Re-dispatch on the outer shell document. We clone the
          // relevant fields so VS Code's keybinding matcher receives
          // an equivalent event. The synthetic event has
          // isTrusted=false, but VS Code's webview keybinding
          // handler matches on key fields rather than the trust
          // flag, so this is enough to make Cmd+J / Cmd+= / Cmd+- /
          // Cmd+B / Cmd+P / Cmd+S / etc. behave the same way as
          // when the focus is in a regular editor.
          const cloned = new KeyboardEvent('keydown', {
            key: e.key,
            code: e.code,
            keyCode: e.keyCode,
            which: e.which,
            ctrlKey: e.ctrlKey,
            metaKey: e.metaKey,
            shiftKey: e.shiftKey,
            altKey: e.altKey,
            repeat: e.repeat,
            bubbles: true,
            cancelable: true,
            composed: true,
          });
          document.dispatchEvent(cloned);
        });
        // Right-click → custom menu in the outer shell. Use mousedown
        // for the dismiss handler ordering; contextmenu still fires
        // after, and we preventDefault to suppress any host menu.
        win.addEventListener('contextmenu', function (e) {
          e.preventDefault();
          const rect = iframe.getBoundingClientRect();
          const x = e.clientX + rect.left;
          const y = e.clientY + rect.top;
          const sel = win.getSelection();
          const hasSel = !!(sel && sel.toString().length > 0);
          // If the user right-clicked on an <img> or a panel-inlined
          // SVG plot, capture the element itself so the Copy image
          // action can draw it onto a canvas (fetch() is blocked by
          // the outer-shell CSP's connect-src 'none', so we read
          // pixels from already-loaded content rather than
          // re-requesting).
          let image = null;
          const tgt = e.target;
          if (tgt && tgt.tagName === 'IMG') image = tgt;
          else if (tgt && tgt.closest) image = tgt.closest('svg.raven-knit-plot-svg');
          showContextMenu(x, y, hasSel, image);
        });
        // A new click inside the iframe should dismiss any open
        // toolbar UI so they don't linger after the user moves on.
        // The HTML popover API gives outside-click dismissal for free,
        // BUT the iframe is sandboxed (allow-same-origin): mousedown
        // events that fire INSIDE the iframe don't propagate to the
        // outer shell document, so the browser's popover light-dismiss
        // logic never sees them and leaves the export popover open.
        // Routing iframe mousedowns through closeExportPopover (and the
        // existing hideContextMenu) makes a click in the rendered
        // report behave the same way a click on the toolbar would —
        // matching the plot viewer's UX where any click outside the
        // share popover closes it.
        function dismissToolbarUi() {
          hideContextMenu();
          closeExportPopover();
        }
        win.addEventListener('mousedown', dismissToolbarUi);
        win.addEventListener('scroll', hideContextMenu, true);
        // Re-attach is required after every iframe reload (Knit
        // again, or singleton-panel content swap).
      }

      iframe.addEventListener('load', attachIframeInputHandlers);
      if (iframe.contentDocument
          && iframe.contentDocument.readyState !== 'loading') {
        attachIframeInputHandlers();
      }
      // Report the webview's actually-rendered editor background to
      // the extension host. The host uses this to identify which
      // theme VS Code is rendering: the public API exposes only
      // activeColorTheme.kind, which is ambiguous when both
      // workbench.preferredLightColorTheme and
      // workbench.preferredDarkColorTheme have the same kind (e.g.
      // both configured to dark themes). The actual editor background
      // is the only public signal that lets the host match the right
      // theme JSON.
      function reportThemeContext() {
        try {
          var cs = getComputedStyle(document.documentElement);
          var bg = (cs.getPropertyValue('--vscode-editor-background') || '').trim();
          if (bg.length > 0) {
            vscode.postMessage({ type: 'themeContext', editorBackground: bg });
          }
        } catch (e) { /* ignore — host falls back to first candidate */ }
      }
      // Initial report — at this point the outer shell has been
      // styled, so the CSS variable is resolved.
      reportThemeContext();
      // Ask the host to (re-)resolve and push the current VS Code
      // theme palette. The host's pushVscodeThemePalette already
      // fires on theme/config events, but on a panel reuse the host
      // sets webview.html and then pushes — that postMessage can
      // race the fresh shell's listener registration and be lost.
      // Pulling once from this fully-booted state guarantees we
      // never see the stale baked palette permanently.
      vscode.postMessage({ type: 'requestPalette' });
      // Same race-avoidance pattern for the live-font override. The
      // initial \`vscodeFontCss\` captured at template-render time is
      // already applied via applyFonts above; this pull asks the host
      // to re-resolve and push the CURRENT value, covering a panel
      // reuse where settings changed between renders.
      vscode.postMessage({ type: 'requestFonts' });
      // Re-apply when VS Code switches its active theme. The outer
      // shell body class flips between vscode-light, vscode-dark, or
      // vscode-high-contrast, which updates the CSS variables read
      // by readThemeColors. Re-report the editor background too so
      // the host re-resolves to the new theme.
      new MutationObserver(function () {
        applyTheme();
        reportThemeContext();
      }).observe(document.body, {
        attributes: true, attributeFilter: ['class'],
      });
      syncThemeBtn();

      // VS Code theme palette updates from the extension host. We
      // accept the css string only when it round-trips through a
      // strict shape check — the field is concatenated into
      // style.textContent, so a malformed value (or one bearing CSS
      // that escapes the var-declaration form) would corrupt the
      // iframe stylesheet. The regex matches the exact declaration
      // sequence paletteCssDeclarations emits; anything else is
      // dropped and the toggle falls back to the GitHub variant.
      //
      // The role-suffix accepts mixed case so future TokenRole names
      // mirroring VS Code semantic-token type names (e.g.
      // 'enumMember') stay representable. The hex literal mirrors
      // vscode-theme-palette.ts:HEX_COLOR_RE — accept only the four
      // CSS-spec hex lengths (3, 4, 6, 8); the wider {3,8} form would
      // silently pass 5/7-digit malformed values that no current code
      // path emits but a future refactor could.
      var RAVEN_PALETTE_CSS_RE = /^(?:--raven-(?:bg|fg|c-[a-zA-Z]+): #(?:[0-9a-fA-F]{3,4}|[0-9a-fA-F]{6,8}); ?)+$/;
      // Names paletteCssDeclarations is contracted to emit, in the
      // SAME order. We assert the entire set is present, with no
      // duplicates and no extras, so a partial payload (which the
      // open-ended (?: ... ; ?)+ above otherwise accepts) cannot
      // silently override the baked GitHub palette with a missing
      // var that then falls back to the OTHER variant. Keep in
      // lockstep with paletteCssDeclarations.
      var RAVEN_PALETTE_REQUIRED_NAMES = [
        '--raven-bg',
        '--raven-fg',
        '--raven-c-keyword',
        '--raven-c-string',
        '--raven-c-number',
        '--raven-c-comment',
        '--raven-c-function',
        '--raven-c-type',
        '--raven-c-variable',
        '--raven-c-operator',
        '--raven-c-punctuation',
        '--raven-c-constant',
      ];
      function paletteCssIsComplete(css) {
        var seen = Object.create(null);
        var pat = /--raven-(?:bg|fg|c-[a-zA-Z]+)(?=:)/g;
        var m;
        while ((m = pat.exec(css)) !== null) {
          if (seen[m[0]]) return false; // dup
          seen[m[0]] = true;
        }
        for (var i = 0; i < RAVEN_PALETTE_REQUIRED_NAMES.length; i++) {
          if (!seen[RAVEN_PALETTE_REQUIRED_NAMES[i]]) return false;
        }
        // Count seen names too — refuse extras. seen is created with
        // Object.create(null), so it has no prototype: every enumerable
        // key is an own property and for...in only visits those. The
        // null prototype also means seen.hasOwnProperty is undefined,
        // so calling it would throw — count keys directly instead.
        var count = 0;
        for (var k in seen) count++;
        return count === RAVEN_PALETTE_REQUIRED_NAMES.length;
      }

      // Font payload accept-regex and required-name set. Mirrors the
      // palette pattern but for the two --raven-font-* declarations.
      //
      // Inner value class \`[^;{}<>\\\\\\n\\r\\t\\f\\v\\0]+\` allows
      // parentheses so font families like "Aptos (Body)" pass validation.
      // Combined with the explicit declaration shape
      // (\`--raven-font-(?:text|mono): \`) and the requirement that BOTH
      // names appear via \`fontCssIsComplete\`, the webview can only
      // inject the precise two-declaration sequence
      // \`fontsCssDeclarations\` is contracted to emit.
      var RAVEN_FONT_CSS_RE = /^(?:--raven-font-(?:text|mono): [^;{}<>\\\\\\n\\r\\t\\f\\v\\0]+; ?){2}$/;
      var RAVEN_FONT_REQUIRED_NAMES = [
        '--raven-font-text',
        '--raven-font-mono',
      ];
      function fontCssIsComplete(css) {
        var seen = Object.create(null);
        var pat = /--raven-font-(?:text|mono)(?=:)/g;
        var m;
        while ((m = pat.exec(css)) !== null) {
          if (seen[m[0]]) return false;
          seen[m[0]] = true;
        }
        for (var j = 0; j < RAVEN_FONT_REQUIRED_NAMES.length; j++) {
          if (!seen[RAVEN_FONT_REQUIRED_NAMES[j]]) return false;
        }
        var count2 = 0;
        for (var k2 in seen) count2++;
        return count2 === RAVEN_FONT_REQUIRED_NAMES.length;
      }
      // Quote-aware bare-paren guard. Mirrors \`render-html.ts\`'s
      // \`hasBareParens\` so the trust boundary stays defensible on its
      // own. The accept-regex above intentionally permits \`(\` and \`)\`
      // so quoted real-world names like \`"Aptos (Body)"\` round-trip;
      // this helper catches the residual hazard — an unquoted \`Foo(\`
      // would open a CSS function-token whose consumption ignores
      // \`}\` boundaries and could corrupt the iframe stylesheet
      // outside the font-override block.
      function fontCssHasBareParens(css) {
        var quote = null;
        for (var i2 = 0; i2 < css.length; i2++) {
          var ch = css.charAt(i2);
          if (quote) {
            if (ch === quote) quote = null;
            continue;
          }
          if (ch === '"' || ch === "'") { quote = ch; continue; }
          if (ch === '(' || ch === ')') return true;
        }
        return false;
      }
      window.addEventListener('message', function (event) {
        var data = event && event.data;
        if (!data) return;
        if (data.__ravenVscodeThemePalette === true) {
          if (typeof data.css === 'string'
              && RAVEN_PALETTE_CSS_RE.test(data.css)
              && paletteCssIsComplete(data.css)) {
            vscodePaletteCss = data.css;
          } else {
            vscodePaletteCss = null;
          }
          applyTheme();
          return;
        }
        if (data.__ravenFontFamilies === true) {
          // Accept only the exact two-declaration shape
          // \`fontsCssDeclarations\` emits, with both required names,
          // no duplicates, no extras, and no bare parens outside
          // quoted family names. Anything else clears the override
          // so the baked fonts in the on-disk .html show through —
          // same fail-safe model as the palette path.
          if (typeof data.css === 'string'
              && RAVEN_FONT_CSS_RE.test(data.css)
              && fontCssIsComplete(data.css)
              && !fontCssHasBareParens(data.css)) {
            vscodeFontCss = data.css;
          } else {
            vscodeFontCss = null;
          }
          applyFonts();
          return;
        }
        if (data.__ravenExportBusy === true) {
          // Host->webview only. The data.busy field is the single
          // source of truth -- coerce to a strict boolean check so a
          // smuggled non-boolean cannot enable the cancel dispatch.
          if (data.busy === true) {
            exportBtn.dataset.busy = 'true';
            // Dismiss any open format popover so the user isn't
            // looking at a fresh "Export to HTML / PDF / Word" menu
            // anchored under a trigger that has just swapped to the
            // cancel icon. Without this, picking a format from the
            // open popover would post requestExport against an op
            // key that's already busy, surfacing a surprise
            // "cancel-and-retry" toast.
            closeExportPopover();
          } else {
            delete exportBtn.dataset.busy;
          }
          syncExportBtn();
          return;
        }
        if (data.__ravenRequestThemeContext === true) {
          // The host asked us to re-report the current editor.bg.
          // Fires on theme changes that the MutationObserver above
          // doesn't catch — specifically, swaps between two themes
          // of the same kind (e.g. Solarized Dark <-> Dark 2026,
          // both kind=Dark). Body class stays the same, so the
          // MutationObserver doesn't fire, so the host's cached
          // latestEditorBackground would stay on the old theme's
          // bg. The host invalidates its cache and then pokes us
          // here; we read the current bg and post it back.
          reportThemeContext();
          return;
        }
        if (data.__ravenKnitProbe !== true) return;
        var locationHref = '';
        try {
          locationHref = iframe.contentWindow ? iframe.contentWindow.location.href : '';
        } catch (e) {
          // SecurityError accessing cross-origin location → iframe
          // navigated to its (cross-origin) src; report that
          // sentinel so the extension treats it as success.
          locationHref = 'cross-origin-blocked';
        }
        // Inspect every <img> in the rendered iframe and report
        // whether the browser actually fetched the bytes. Used by
        // the diagnostic test that gates regressions for the
        // "subresource loads from a nested iframe inside a VS Code
        // webview" failure mode.
        var imageStates = [];
        try {
          var idoc = iframe.contentDocument;
          if (idoc) {
            var imgs = idoc.querySelectorAll('img');
            for (var i = 0; i < imgs.length; i++) {
              var im = imgs[i];
              imageStates.push({
                src: im.getAttribute('src') || '',
                resolvedSrc: im.src || '',
                complete: !!im.complete,
                naturalWidth: im.naturalWidth || 0,
                naturalHeight: im.naturalHeight || 0,
              });
            }
          }
        } catch (e) { /* same-origin failure — leave empty */ }
        vscode.postMessage({
          type: 'iframeProbe',
          locationHref: locationHref,
          loadFired: loadFired,
          errorFired: errorFired,
          src: iframe.getAttribute('src'),
          imageStates: imageStates,
        });
      });
      // Surface CSP violations so the test/diagnostic layer can
      // distinguish "blocked by CSP" from "blocked by network".
      window.addEventListener('securitypolicyviolation', function (e) {
        vscode.postMessage({
          type: 'cspViolation',
          violatedDirective: String(e.violatedDirective || ''),
          blockedURI: String(e.blockedURI || ''),
        });
      });
    })();
  </script>
</body>
</html>`;
}

/**
 * Rewrite fragment-only anchor hrefs (`<a href="#x">`) so they target
 * `about:srcdoc#x` — the srcdoc iframe's actual document URL.
 *
 * Why this is needed: the outer-shell injects a `<base href>` so the
 * rendered HTML's relative subresource paths (CSS, images, fonts)
 * resolve through `webview.asWebviewUri(...)`. But the base href also
 * changes how fragment-only anchors are resolved — instead of
 * resolving against the iframe's document URL (`about:srcdoc`), they
 * resolve against the base URL, which turns a click on a TOC link
 * into a full cross-document navigation that fails (the nested-frame
 * navigation issue this whole module exists to work around).
 *
 * Rewriting `href="#x"` to `href="about:srcdoc#x"` produces a URL
 * whose non-fragment portion already matches the iframe's document
 * URL, so the browser treats the click as a same-document fragment
 * navigation again and scrolls to the target element.
 *
 * Edge cases NOT rewritten (intentionally):
 *  - `href="page.html#x"` — combined path+fragment, not a pure
 *    in-document anchor.
 *  - `href=""` or `href="#"` — empty or no-op anchors.
 *  - Non-`<a>` elements that happen to have an `href` attribute.
 *  - `href` values containing `<`, `>`, or whitespace — those are
 *    pathological and reject rather than rewrite.
 */
export function rewriteFragmentAnchors(html: string): string {
    // Match `<a ...href="#fragment"...>` and `<a ...href='#fragment'...>`.
    // The lookahead-free pattern matches `<a` followed by anything up
    // to `href=`, then a quoted `#…` value. `[^>]*?` is non-greedy so
    // the regex does not jump across `>` boundaries.
    const re = /(<a\b[^>]*?\shref\s*=\s*)("|')(#[^"'<>\s]+)\2/gi;
    return html.replace(re, (_match, prefix: string, quote: string, fragment: string) =>
        `${prefix}${quote}about:srcdoc${fragment}${quote}`,
    );
}

/**
 * Pick the output path to surface in the Knit Preview panel.
 *
 * When `output_format = "all"` (or a custom multi-format render) produces
 * a mix of formats, the user almost always wants the HTML viewer rather
 * than e.g. revealing a PDF in the file browser. Prefer the first HTML
 * output; fall back to the first entry overall.
 *
 * Codex adversarial review #4 on the v1 spec called out that v1 always
 * used `parsed.paths[0]`, which would hide an HTML output behind a
 * PDF/DOCX-first reveal.
 */
export function pickPrimaryOutput(paths: readonly string[]): string | undefined {
    if (paths.length === 0) return undefined;
    const html = paths.find((p) => {
        const ext = path.extname(p).toLowerCase();
        return ext === '.html' || ext === '.htm';
    });
    return html ?? paths[0];
}
