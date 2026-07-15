/**
 * Resolves the active VS Code color theme into a `GithubPalette`-shaped
 * record so the Knit Preview webview can paint syntax tokens with the
 * editor's actual colors instead of the bundled GitHub palette.
 *
 * Why this lives in a dedicated module:
 *
 *   - VS Code's public extension API does NOT expose the active
 *     theme's TextMate `tokenColors`. The only exposed signal is
 *     `ColorThemeKind` (Light / Dark / HC / HC-Light) plus the named
 *     CSS variables a webview can read from the shell document. To
 *     resolve real per-scope colors we have to find the contributing
 *     extension's theme JSON on disk, parse it (including `include`
 *     chains), and layer the user's `editor.tokenColorCustomizations`
 *     on top. None of that is shared with the grammar loader.
 *
 *   - Scope-selector matching is genuinely complex (parent selectors,
 *     specificity, comma-separated alternatives, rule ordering). We
 *     delegate to vscode-textmate's own matcher via
 *     `GrammarRegistry.extractWithTheme` — set the merged theme on the
 *     registry, tokenize a canonical R snippet per role, read the
 *     foreground color index out of the binary-tokenize result, look
 *     up the hex in the registry's color map. That way we never
 *     re-implement scope matching here.
 *
 *   - The extractor is purely async + dependency-injected (no `vscode`
 *     module import). The live wiring lives in `knit-output-panel.ts`,
 *     which feeds in `extensions`, the workbench theme id, user
 *     customizations, and a reader function. Unit tests fake all
 *     three.
 *
 * Failure modes are explicit (`{ok: false, reason, detail}`) so callers
 * can decide whether to fall back, log, or surface to the user. The
 * panel wiring always falls back silently to the GitHub palette and
 * logs one line to the Knit output channel.
 */

import * as path from 'path';
import {
    githubLight,
    githubDark,
    scopeToRole,
    type GithubPalette,
    type TokenRole,
} from './github-colors';
import type { GrammarRegistry, ScopeToken, ThemeSetting } from './grammar-registry';

/**
 * Minimal `vscode.Extension` surface the extractor consumes. Defined
 * inline so the module has no `vscode` import — callers pass
 * `{id, extensionPath, packageJSON}` shaped objects.
 */
export interface ExtensionLike {
    id: string;
    extensionPath: string;
    packageJSON: unknown;
}

export type ThemeFailureReason =
    | 'no-theme-id'
    | 'theme-not-found'
    | 'read-error'
    | 'parse-error'
    | 'unsupported-format'
    | 'cycle-detected'
    | 'grammar-unavailable';

export interface ThemePaletteSuccess {
    ok: true;
    palette: GithubPalette;
    /** The matched theme id (or label fallback). */
    themeId: string;
    /** Display label used by VS Code's per-theme customization keys. */
    themeLabel?: string;
    /** Whether the theme is light. From the caller's ColorThemeKind. */
    isLight: boolean;
    /**
     * Per-candidate failures encountered before reaching the picked
     * theme. Empty if the first candidate loaded cleanly. Surfaced so
     * the panel can log a user-visible breadcrumb like
     * "Tokyo Night Light failed (parse-error): ...; fell back to
     * Dark 2026". Without this, a quietly-failing first candidate
     * looks indistinguishable from "the user only configured one
     * theme".
     */
    candidateFailures: ReadonlyArray<{
        themeId: string;
        reason: ThemeFailureReason;
        detail: string;
    }>;
}

export interface ThemePaletteFailure {
    ok: false;
    reason: ThemeFailureReason;
    detail: string;
}

export type ThemePaletteOutcome = ThemePaletteSuccess | ThemePaletteFailure;

export interface ThemePaletteArgs {
    /**
     * Ordered candidate theme ids — try each in turn. When
     * `activeEditorBackground` is also provided, the resolver picks
     * the first candidate whose theme JSON's `colors.editor.background`
     * matches it; otherwise it uses the first candidate that loads
     * successfully.
     *
     * Why a list and not a single id: `vscode.window.activeColorTheme`
     * exposes only `kind`, not the active theme's id. When
     * `window.autoDetectColorScheme` is on, VS Code picks between
     * `workbench.preferredLightColorTheme` and
     * `workbench.preferredDarkColorTheme` based on the OS appearance —
     * a signal the public API does NOT surface. If both preferreds
     * happen to have the same kind (e.g. a user who's configured both
     * to dark themes), neither `kind` nor any other setting can
     * disambiguate. The webview's actually-rendered
     * `--vscode-editor-background` is the ground truth, so we lean on
     * it.
     */
    candidateThemeIds: readonly string[];

    /**
     * The webview's actual `--vscode-editor-background`, read with
     * `getComputedStyle`. Used to disambiguate among
     * `candidateThemeIds` when settings alone aren't enough. Hex
     * format; case-insensitive. Optional — when omitted, the first
     * candidate is used.
     */
    activeEditorBackground?: string;

    /**
     * Whether the active theme is light. Comes from
     * `vscode.window.activeColorTheme.kind` — the API exposes this
     * directly, so we never guess from the theme JSON's `"type"` field
     * or the editor background's luminance.
     */
    isLight: boolean;

    /** All installed extensions, for the `contributes.themes` walk. */
    extensions: readonly ExtensionLike[];

    /**
     * Raw value of `editor.tokenColorCustomizations`. Accepts the
     * fallback shape (`{textMateRules: […]}`) and the per-theme shape
     * (`{"[Default Dark+]": {textMateRules: […]}}`). Both are layered
     * after the resolved theme's `tokenColors`.
     */
    tokenColorCustomizations: unknown;

    /**
     * Raw value of `editor.semanticTokenColorCustomizations`. Used for
     * the simple form only (a top-level `rules` map of
     * `type[.modifier]` → color or `{foreground}`). The general
     * type+modifier selector grammar is not implemented; complex
     * selectors are ignored.
     */
    semanticTokenColorCustomizations: unknown;

    /**
     * Grammar registry to drive scope-selector matching. The registry
     * must be capable of tokenizing the R language; otherwise we
     * return `grammar-unavailable`. We use both `tokenizeLineForLanguage`
     * (to recover per-token scope chains; theme-independent) and
     * `extractWithTheme` (to apply the merged theme and read per-
     * token foreground colors via `tokenizeLine2`).
     */
    registry: Pick<GrammarRegistry, 'extractWithTheme' | 'primeForLanguage' | 'tokenizeLineForLanguage'>;

    /** File reader for theme JSONs. Absolute paths only. */
    readFile: (absolutePath: string) => Promise<string>;

    /**
     * realpath resolver. Used for theme-include cycle detection and
     * for stable canonicalization across symlinked extension installs.
     * Defaults to `(p) => Promise.resolve(p)` for tests; production
     * passes `fs.promises.realpath`.
     */
    realPath?: (absolutePath: string) => Promise<string>;
}

/**
 * A small R-code corpus that exercises tokens of every `TokenRole`
 * across the variety of scope chains a real R grammar produces.
 *
 * Why a corpus and not a canonical probe-per-role:
 *
 * Themes don't all target the same scopes. Some style
 * `entity.name.function` (function declarations), others target
 * `support.function` (builtins), yet others rely on the broader
 * `meta.function-call`. A single canonical probe per role can only
 * land on ONE of those scopes — whichever it doesn't pick, the
 * theme's specific styling for it is invisible to us.
 *
 * Instead, the corpus contains a handful of representative lines
 * whose tokenization (by the active R grammar) yields a mix of the
 * usual TextMate scopes. We then ask vscode-textmate to color each
 * token under the active theme, map each token's scope chain to a
 * `TokenRole` via the same `scopeToRole` the highlighter uses, and
 * tally colors per role. The most-voted non-default color wins.
 *
 * This way:
 *
 *   - Themes that style `support.function` color `library` / `summary` /
 *     `list` tokens.
 *   - Themes that style `entity.name.function` color the `foo` in
 *     `foo <- function(...)`.
 *   - Themes that style `meta.function-call` color every function-call
 *     identifier whether builtin or not.
 *   - Themes that style `variable.parameter` color named arguments.
 *
 * The "no rule matched" fall-through color (either an empty-scope
 * default rule's foreground or vscode-textmate's hardcoded `#000000`)
 * is filtered out per-vote, so it can never win a role even if it
 * appears more often than any specific color.
 */
const SAMPLE_CORPUS: readonly string[] = [
    '# a representative comment',
    'library(ggplot2)',
    'data <- mtcars',
    'summary(data$mpg)',
    'square <- function(arg) { arg * 2 }',
    'if (TRUE) NULL else FALSE',
    'list(1, 2.5e-3, "text")',
    'pkg::fun(name = value)',
];

/**
 * Bit layout encoded into `Uint32Array` metadata by vscode-textmate's
 * `tokenizeLine2`. The foreground color index occupies bits 15..23.
 * The constant matches vscode-textmate's MetadataConsts; if upstream
 * ever shifts the layout the test suite would catch it because the
 * canonical-scope probes return obviously-wrong colors.
 */
const FOREGROUND_OFFSET = 15;
const FOREGROUND_MASK = 0x00FF8000;

/** Match any of `#rrggbb`, `#rgb`, `#rrggbbaa`, or `#rgba`. */
const HEX_COLOR_RE = /^#(?:[0-9a-f]{3,4}|[0-9a-f]{6,8})$/i;

/**
 * Normalize a CSS hex color into a canonical `#rrggbb` (no alpha) lowercase
 * form for cross-source equality comparison. Returns `undefined` if the
 * input is missing / not a valid hex literal.
 *
 * Why this exists: the theme JSON's `colors.editor.background` and the
 * webview-reported `--vscode-editor-background` are both valid hex, but
 * they can encode the same color in different lengths:
 *
 *   - 3- and 4-digit shorthands (`#1e1`, `#1e1f`) → 6-digit expansion.
 *   - 8-digit RGBA (`#1e1e1eff`) vs 6-digit (`#1e1e1e`) when the alpha is
 *     fully opaque.
 *   - Lowercase vs uppercase hex.
 *   - Stray surrounding whitespace from a hand-edited theme.
 *
 * The disambiguation logic compares the two for strict equality; without
 * normalization the bg-match path silently fails and the resolver falls
 * back to `firstLoaded` — which is exactly wrong in the autoDetect +
 * same-kind preferred-* scenario the bg-match path exists to handle.
 *
 * Alpha is intentionally dropped when fully opaque. A theme that paints
 * its editor background semi-transparently is a real (if rare) case; for
 * disambiguation purposes we treat `#1e1e1e80` as distinct from
 * `#1e1e1e` (different colors on screen), but `#1e1e1eff` as equal to
 * `#1e1e1e` (identical colors).
 */
function normalizeBgForCompare(raw: string | undefined): string | undefined {
    if (typeof raw !== 'string') return undefined;
    const trimmed = raw.trim().toLowerCase();
    if (!HEX_COLOR_RE.test(trimmed)) return undefined;
    // Expand 3- / 4-digit shorthands to their 6- / 8-digit forms by
    // duplicating each hex digit, then drop fully-opaque alpha.
    let body = trimmed.slice(1);
    if (body.length === 3 || body.length === 4) {
        body = body.split('').map((c) => c + c).join('');
    }
    if (body.length === 8 && body.slice(6) === 'ff') {
        body = body.slice(0, 6);
    }
    return '#' + body;
}

/**
 * Semantic-token type name → `TokenRole` mapping for the simple
 * `semanticTokenColors` form. Bare type names only — selectors with
 * modifiers (`function.declaration`) are dropped on the floor by the
 * caller, which doesn't implement the full VS Code semantic-token
 * selector grammar. The map covers the standard semantic-token types
 * VS Code's API documents plus a few commonly-used aliases.
 */
const SEMANTIC_TYPE_TO_ROLE: Readonly<Record<string, TokenRole>> = {
    keyword: 'keyword',
    string: 'string',
    number: 'number',
    comment: 'comment',
    function: 'function',
    method: 'function',
    type: 'type',
    class: 'type',
    interface: 'type',
    struct: 'type',
    variable: 'variable',
    parameter: 'variable',
    property: 'variable',
    operator: 'operator',
    namespace: 'type',
    macro: 'function',
    enum: 'type',
    enumMember: 'constant',
    decorator: 'function',
};

/**
 * Resolve the active VS Code theme into a Knit-Output-ready palette.
 *
 * Top-level flow:
 *
 *   1. Locate the contributing theme JSON via `contributes.themes`.
 *   2. Read + parse, resolving `include` chains relative to each
 *      including file (cycle detection via canonicalized absolute
 *      paths).
 *   3. Layer the user's `editor.tokenColorCustomizations` after the
 *      resolved theme's `tokenColors` — both the global fallback
 *      block and any `"[<active-theme-label>]"` block.
 *   4. Layer semantic-color customizations after that.
 *   5. Apply the merged settings via `registry.extractWithTheme`,
 *      tokenize one probe snippet per `TokenRole`, decode the
 *      foreground color index from the binary token at the probe
 *      offset, look up the hex in `colorMap`. Roles whose probe
 *      yields the default foreground (index 0) fall back to another
 *      role's resolved color if a `fallbackRole` is configured, then
 *      ultimately to the GitHub palette underneath.
 *   6. Layer `semanticTokenColors.function` (and any other
 *      type-name → color entries that map to one of our roles) on
 *      top. This makes themes that customize semantic colors look
 *      consistent with VS Code's editor.
 *   7. Validate every resolved color via `HEX_COLOR_RE` and reject
 *      invalid entries (they fall back to the GitHub palette).
 */
export async function resolveActiveThemePalette(
    args: ThemePaletteArgs,
): Promise<ThemePaletteOutcome> {
    if (!args.candidateThemeIds || args.candidateThemeIds.length === 0) {
        return { ok: false, reason: 'no-theme-id', detail: 'no candidate theme ids supplied' };
    }

    const wantedBg = normalizeBgForCompare(args.activeEditorBackground);
    const realPath = args.realPath ?? ((p) => Promise.resolve(p));

    // Walk candidates in order. For each, try to locate + parse. If
    // `wantedBg` is set, prefer the candidate whose theme JSON has a
    // matching `colors.editor.background` — that's how we know which
    // of multiple same-kind candidates VS Code is actually rendering.
    // If no candidate's bg matches (or there's no bg hint), use the
    // first one that loaded successfully. Track the most relevant
    // failure so we can report a useful error if every candidate
    // fails.
    let firstLoaded:
        | { located: LocatedTheme; mergedDoc: ParsedThemeDocument }
        | null = null;
    let bgMatched:
        | { located: LocatedTheme; mergedDoc: ParsedThemeDocument }
        | null = null;
    const candidateFailures: Array<{ themeId: string; reason: ThemeFailureReason; detail: string }> = [];

    for (const candidate of args.candidateThemeIds) {
        if (!candidate) continue;
        const located = locateThemeFile(candidate, args.extensions);
        if (!located) {
            candidateFailures.push({
                themeId: candidate,
                reason: 'theme-not-found',
                detail: `no contributed theme matches "${candidate}"`,
            });
            continue;
        }
        let mergedDoc: ParsedThemeDocument;
        try {
            mergedDoc = await loadAndMergeThemeChain({
                entryPath: located.absolutePath,
                extensionRoot: located.extensionRoot,
                readFile: args.readFile,
                realPath,
            });
        } catch (err) {
            const reason: ThemeFailureReason =
                err instanceof UnsupportedFormatError ? 'unsupported-format'
                : err instanceof ThemeCycleError ? 'cycle-detected'
                : err instanceof ParseError ? 'parse-error'
                : 'read-error';
            candidateFailures.push({
                themeId: candidate,
                reason,
                detail: err instanceof Error ? err.message : String(err),
            });
            continue;
        }
        firstLoaded ??= { located, mergedDoc };
        if (wantedBg) {
            const themeBg = normalizeBgForCompare(mergedDoc.editorColors['editor.background']);
            if (themeBg && themeBg === wantedBg) {
                bgMatched = { located, mergedDoc };
                break;
            }
        }
    }

    const picked = bgMatched ?? firstLoaded;
    if (!picked) {
        // All candidates failed. Use the last failure (most-recent
        // attempt) for the surfaced reason — usually the most useful
        // because earlier failures may be "theme not installed"
        // boilerplate.
        const last = candidateFailures[candidateFailures.length - 1];
        return {
            ok: false,
            reason: last?.reason ?? 'theme-not-found',
            detail: last?.detail ?? `none of [${args.candidateThemeIds.join(', ')}] resolved`,
        };
    }
    const located = picked.located;
    const mergedDoc = picked.mergedDoc;

    const tokenColors = synthesizeDefaultRule(
        mergeCustomizations({
            baseTokenColors: mergedDoc.tokenColors,
            tokenColorCustomizations: args.tokenColorCustomizations,
            activeThemeLabel: located.label,
        }),
        mergedDoc.editorColors,
    );

    // Probe via vscode-textmate using the merged TextMate settings.
    const rGrammarReady = await args.registry.primeForLanguage('r');
    if (!rGrammarReady) {
        return {
            ok: false,
            reason: 'grammar-unavailable',
            detail: 'no R grammar contribution available',
        };
    }

    let resolvedRoleColors: Partial<Record<TokenRole, string>>;
    try {
        resolvedRoleColors = await probeRoleColors({
            registry: args.registry,
            tokenColors,
        });
    } catch (err) {
        return {
            ok: false,
            reason: 'grammar-unavailable',
            detail: err instanceof Error ? err.message : String(err),
        };
    }

    // Semantic-color customizations override the TextMate probe for
    // roles whose type name matches.
    const semanticOverrides = readSemanticOverrides({
        baseSemanticTokenColors: mergedDoc.semanticTokenColors,
        semanticTokenColorCustomizations: args.semanticTokenColorCustomizations,
        activeThemeLabel: located.label,
    });
    for (const [role, color] of Object.entries(semanticOverrides) as Array<[TokenRole, string]>) {
        if (color) resolvedRoleColors[role] = color;
    }

    // Validate every role color; drop invalid ones.
    for (const role of Object.keys(resolvedRoleColors) as TokenRole[]) {
        const color = resolvedRoleColors[role];
        if (!color || !HEX_COLOR_RE.test(color)) delete resolvedRoleColors[role];
    }

    // Background / foreground come from the merged theme's editor
    // colors. Sanitize via the same hex regex.
    const bg = pickValidColor(mergedDoc.editorColors['editor.background']);
    const fg = pickValidColor(mergedDoc.editorColors['editor.foreground']);

    // For roles with no extracted color, fall back to `noMatchFg` —
    // the same color vscode-textmate paints any token whose scope
    // chain doesn't match a specific rule, i.e. the color the EDITOR
    // shows for those tokens. Falling back to the GitHub palette
    // would introduce hues the editor never produces.
    const fallback = args.isLight ? githubLight : githubDark;
    const noMatchFg = effectiveDefaultForeground(tokenColors);
    const roleFallback = HEX_COLOR_RE.test(noMatchFg) ? noMatchFg : (fg ?? fallback.foreground);
    const palette: GithubPalette = {
        background: bg ?? fallback.background,
        foreground: fg ?? fallback.foreground,
        roles: {
            keyword: resolvedRoleColors.keyword ?? roleFallback,
            string: resolvedRoleColors.string ?? roleFallback,
            number: resolvedRoleColors.number ?? roleFallback,
            comment: resolvedRoleColors.comment ?? roleFallback,
            function: resolvedRoleColors.function ?? roleFallback,
            type: resolvedRoleColors.type ?? roleFallback,
            variable: resolvedRoleColors.variable ?? roleFallback,
            operator: resolvedRoleColors.operator ?? roleFallback,
            punctuation: resolvedRoleColors.punctuation ?? roleFallback,
            constant: resolvedRoleColors.constant ?? roleFallback,
        },
    };

    return {
        ok: true,
        palette,
        themeId: located.id,
        themeLabel: located.label,
        isLight: args.isLight,
        candidateFailures,
    };
}

// ---------------------------------------------------------------------
// Theme-file lookup
// ---------------------------------------------------------------------

interface LocatedTheme {
    id: string;
    label: string;
    absolutePath: string;
    /**
     * Extension root that contributed this theme. Used downstream to
     * keep `include` chains inside the contributing extension's
     * directory.
     */
    extensionRoot: string;
}

interface ThemeContribution {
    id?: string;
    label?: string;
    uiTheme?: string;
    path?: string;
}

/**
 * Walk `extensions` for a `contributes.themes` entry whose `id` (or,
 * failing that, `label`) matches `wantedId`. The setting
 * `workbench.colorTheme` typically holds the label (e.g.
 * "Default Dark Modern"), but some users persist the `id` after a
 * VS Code-internal upgrade — so we accept either as the lookup key.
 */
function locateThemeFile(
    wantedId: string,
    extensions: readonly ExtensionLike[],
): LocatedTheme | null {
    for (const ext of extensions) {
        const themes = readThemeContributions(ext.packageJSON);
        for (const t of themes) {
            const id = typeof t.id === 'string' ? t.id : '';
            const label = typeof t.label === 'string' ? t.label : '';
            if (id !== wantedId && label !== wantedId) continue;
            if (typeof t.path !== 'string' || t.path.length === 0) continue;
            const absolutePath = path.isAbsolute(t.path)
                ? t.path
                : path.join(ext.extensionPath, t.path);
            // Defense-in-depth: reject contributions whose theme path
            // escapes the extension directory. VS Code's contribution
            // schema does not validate this either, but the theme file
            // ends up being `readFile`'d and its absolute path is
            // logged to the output channel on parse failure — an
            // accidental or malicious `../../etc/passwd` would surface
            // there.
            if (!isInsideDir(ext.extensionPath, absolutePath)) continue;
            return {
                id: id || label || wantedId,
                label: label || id,
                absolutePath,
                extensionRoot: ext.extensionPath,
            };
        }
    }
    return null;
}

/**
 * True when `child` resolves to a path equal to or under `parent`. Both
 * arguments must be absolute. Used to keep theme contribution paths and
 * `include` chains inside the contributing extension's directory.
 */
function isInsideDir(parent: string, child: string): boolean {
    const rel = path.relative(parent, child);
    if (rel === '') return true;
    if (rel.startsWith('..')) return false;
    return !path.isAbsolute(rel);
}

function readThemeContributions(pkg: unknown): ThemeContribution[] {
    if (!pkg || typeof pkg !== 'object') return [];
    const contributes = (pkg as { contributes?: unknown }).contributes;
    if (!contributes || typeof contributes !== 'object') return [];
    const themes = (contributes as { themes?: unknown }).themes;
    if (!Array.isArray(themes)) return [];
    const out: ThemeContribution[] = [];
    for (const t of themes) {
        if (t && typeof t === 'object') out.push(t as ThemeContribution);
    }
    return out;
}

// ---------------------------------------------------------------------
// Theme-JSON parsing + include chain
// ---------------------------------------------------------------------

interface ParsedThemeDocument {
    tokenColors: ThemeSetting[];
    semanticTokenColors: Record<string, unknown>;
    editorColors: Record<string, string>;
}

class ParseError extends Error {}
class UnsupportedFormatError extends Error {}
class ThemeCycleError extends Error {}

/**
 * Load a theme JSON document, recursively resolving `include` chains.
 *
 *   - `tokenColors` arrays from this file are appended AFTER the
 *     included file's, so the current file's rules take precedence
 *     at scope-selector match time (later rules win in TextMate).
 *   - `semanticTokenColors` and `colors` maps are merged with the
 *     current file's keys overriding the included file's.
 *   - `include` paths are resolved relative to the INCLUDING file's
 *     directory, not the original extension root. Built-in themes
 *     like `dark_plus.json` chain through several files this way.
 *   - Cycle detection canonicalizes paths via `realPath`. Symlinked
 *     installs (Homebrew on macOS, some Linux distros) place
 *     extensions behind symlinks that would otherwise let two
 *     `include`s reach the same file via different paths and never
 *     trigger the visited check.
 */
async function loadAndMergeThemeChain(args: {
    entryPath: string;
    /**
     * Contributing extension's root directory. `include` paths that
     * resolve outside this directory are rejected with a ParseError —
     * the schema doesn't mention this constraint, but a theme reaching
     * outside its own extension is almost always either a bug or
     * adversarial.
     */
    extensionRoot: string;
    readFile: (absolutePath: string) => Promise<string>;
    realPath: (absolutePath: string) => Promise<string>;
}): Promise<ParsedThemeDocument> {
    // True cycle detection requires "currently on the include stack"
    // (a back-edge in DFS terms), not "ever seen". A theme can legally
    // be a DAG with diamonds — VS Code's built-in `dark_plus.json`
    // chain pulls in multiple sub-files that share common ancestors.
    // Treating a diamond as a cycle would throw spuriously and force
    // the GitHub-palette fallback.
    const onStack = new Set<string>();
    const alreadyMerged = new Set<string>();
    // Stable canonical-path cache. If realPath fails on the first
    // visit and succeeds on a later one (or vice-versa), the canonical
    // keys would diverge and cycle detection could miss a back-edge.
    // Memoize the first answer per literal path so every later visit
    // produces the same key, even when realPath becomes intermittently
    // unavailable (transient EACCES on a permission-restricted symlink
    // mid-chain, remote workspace blips, etc.).
    const canonicalCache = new Map<string, string>();
    async function canonicalize(p: string): Promise<string> {
        const cached = canonicalCache.get(p);
        if (cached !== undefined) return cached;
        const resolved = await args.realPath(p).catch(() => p);
        canonicalCache.set(p, resolved);
        return resolved;
    }
    // Acc starts with the most-derived (innermost) file's lists, but
    // tokenColors are then prepended by the includes so the final
    // order is "outermost (oldest) → innermost (most-derived)". That
    // matches TextMate semantics: later rules win during selector
    // matching.
    const tokenColors: ThemeSetting[] = [];
    const semanticTokenColors: Record<string, unknown> = {};
    const editorColors: Record<string, string> = {};

    async function load(thisPath: string): Promise<void> {
        const canonical = await canonicalize(thisPath);
        if (onStack.has(canonical)) {
            throw new ThemeCycleError(`include cycle detected at ${canonical}`);
        }
        if (alreadyMerged.has(canonical)) {
            // Diamond include — same file reached via two distinct
            // paths but not via a back-edge. Skip the re-merge so
            // we don't duplicate its rules, but do NOT throw.
            return;
        }
        onStack.add(canonical);

        let raw: string;
        try {
            raw = await args.readFile(thisPath);
        } catch (err) {
            throw new Error(`failed to read ${thisPath}: ${err instanceof Error ? err.message : String(err)}`);
        }

        const trimmed = raw.trimStart();
        if (trimmed.startsWith('<?xml') || trimmed.startsWith('<plist')) {
            throw new UnsupportedFormatError(`tmTheme (XML/plist) format not supported: ${thisPath}`);
        }

        let parsed: unknown;
        try {
            parsed = JSON.parse(stripJsonWithComments(raw));
        } catch (err) {
            throw new ParseError(
                `${thisPath}: ${err instanceof Error ? err.message : String(err)}`,
            );
        }
        if (!parsed || typeof parsed !== 'object') {
            throw new ParseError(`${thisPath}: top-level is not an object`);
        }

        const obj = parsed as Record<string, unknown>;

        // Resolve includes FIRST so the current file's rules can
        // override them. Containment-check against the contributing
        // extension's root so a malformed/adversarial include can't
        // make us read an arbitrary file on disk.
        if (typeof obj.include === 'string' && obj.include.length > 0) {
            const includedAbs = path.resolve(path.dirname(thisPath), obj.include);
            if (!isInsideDir(args.extensionRoot, includedAbs)) {
                throw new ParseError(
                    `${thisPath}: include path "${obj.include}" escapes the extension directory`,
                );
            }
            await load(includedAbs);
        }

        // tokenColors: append (later rules win in TextMate selector
        // matching, which is the order we want — included file's
        // rules first, this file's last).
        const tc = obj.tokenColors;
        if (Array.isArray(tc)) {
            for (const entry of tc) {
                const normalized = normalizeThemeSetting(entry);
                if (normalized) tokenColors.push(normalized);
            }
        }

        // semanticTokenColors: current file overrides included.
        const stc = obj.semanticTokenColors;
        if (stc && typeof stc === 'object' && !Array.isArray(stc)) {
            for (const [k, v] of Object.entries(stc as Record<string, unknown>)) {
                semanticTokenColors[k] = v;
            }
        }

        // colors: current file overrides included.
        const colors = obj.colors;
        if (colors && typeof colors === 'object' && !Array.isArray(colors)) {
            for (const [k, v] of Object.entries(colors as Record<string, unknown>)) {
                if (typeof v === 'string') editorColors[k] = v;
            }
        }

        // Pop from the in-progress stack now that we're done merging
        // this node; promote to `alreadyMerged` so a sibling include
        // path that reaches the same file later sees the deduplication
        // skip (and doesn't replay the rules).
        onStack.delete(canonical);
        alreadyMerged.add(canonical);
    }

    await load(args.entryPath);

    return { tokenColors, semanticTokenColors, editorColors };
}

function normalizeThemeSetting(entry: unknown): ThemeSetting | null {
    if (!entry || typeof entry !== 'object') return null;
    const e = entry as Record<string, unknown>;
    const settings = e.settings;
    if (!settings || typeof settings !== 'object') return null;
    let scope: string | string[] | undefined;
    if (typeof e.scope === 'string') {
        scope = e.scope;
    } else if (Array.isArray(e.scope)) {
        // Filter non-string elements: vscode-textmate's parseTheme
        // calls `.trim()` on every selector string and would throw a
        // TypeError on a null/undefined/number entry inside the array.
        // The throw lands inside the serialized extractWithTheme window
        // and aborts the whole palette resolution; defending here keeps
        // a single malformed rule from poisoning the rest.
        scope = (e.scope as unknown[]).filter((s): s is string => typeof s === 'string');
    }
    return {
        name: typeof e.name === 'string' ? e.name : undefined,
        scope,
        settings: settings as ThemeSetting['settings'],
    };
}

/**
 * Strip line/block comments AND trailing commas from JSON-with-comments.
 * VS Code's built-in themes (and many community themes) ship `.jsonc`
 * files with both — `//`, `/* *\/` blocks, and stray `,` before the
 * closing `}` or `]` of an object or array (e.g. Tokyo Night Light).
 *
 * Two passes to keep each pass simple. Pass 1 removes comments while
 * preserving string literals verbatim. Pass 2 walks the comment-free
 * source and drops any `,` whose next non-whitespace character is
 * `}` or `]`. Doing it in two passes (rather than one) means the
 * trailing-comma check never needs to consider whether the comma sits
 * between a value and a comment vs between a value and the closing
 * brace — the comment is already gone.
 *
 * The stripper is JSONC-lite: it doesn't handle every edge case the
 * jsonc-parser library does (e.g. it doesn't recognize unescaped
 * newlines in strings — but standard JSON forbids those anyway). For
 * the themes we've encountered, this is enough.
 */
function stripJsonWithComments(raw: string): string {
    // Strip a UTF-8 BOM if present. Some Windows-authored theme JSONs
    // ship with a leading ﻿; Node's `fs.readFile(..., 'utf-8')`
    // preserves it, and JSON.parse rejects BOM-prefixed input.
    const withoutBom = raw.charCodeAt(0) === 0xFEFF ? raw.slice(1) : raw;
    return stripTrailingCommas(stripJsonComments(withoutBom));
}

function stripJsonComments(raw: string): string {
    const out: string[] = [];
    let i = 0;
    const n = raw.length;
    while (i < n) {
        const ch = raw[i];
        // String literal — copy verbatim including escaped quotes.
        if (ch === '"') {
            const start = i;
            i++;
            while (i < n) {
                if (raw[i] === '\\' && i + 1 < n) {
                    i += 2;
                    continue;
                }
                if (raw[i] === '"') { i++; break; }
                i++;
            }
            out.push(raw.slice(start, i));
            continue;
        }
        if (ch === '/' && i + 1 < n) {
            const next = raw[i + 1];
            if (next === '/') {
                // Skip to end of line.
                i += 2;
                while (i < n && raw[i] !== '\n') i++;
                continue;
            }
            if (next === '*') {
                i += 2;
                while (i + 1 < n && !(raw[i] === '*' && raw[i + 1] === '/')) i++;
                i = Math.min(n, i + 2);
                continue;
            }
        }
        out.push(ch);
        i++;
    }
    return out.join('');
}

/**
 * Drop trailing commas — any `,` whose next non-whitespace character
 * is `}` or `]`. String-aware so we never strip a comma inside a
 * string literal (e.g. a value like `"foo, bar"`).
 */
function stripTrailingCommas(raw: string): string {
    const out: string[] = [];
    let i = 0;
    const n = raw.length;
    while (i < n) {
        const ch = raw[i];
        if (ch === '"') {
            const start = i;
            i++;
            while (i < n) {
                if (raw[i] === '\\' && i + 1 < n) { i += 2; continue; }
                if (raw[i] === '"') { i++; break; }
                i++;
            }
            out.push(raw.slice(start, i));
            continue;
        }
        if (ch === ',') {
            let j = i + 1;
            while (j < n && (raw[j] === ' ' || raw[j] === '\t' || raw[j] === '\n' || raw[j] === '\r')) j++;
            if (j < n && (raw[j] === '}' || raw[j] === ']')) {
                // Trailing comma — skip.
                i++;
                continue;
            }
        }
        out.push(ch);
        i++;
    }
    return out.join('');
}

// ---------------------------------------------------------------------
// User customizations
// ---------------------------------------------------------------------

/**
 * Layer `editor.tokenColorCustomizations` onto `baseTokenColors`.
 *
 * Precedence (lowest-to-highest, which is FIRST → LAST in TextMate's
 * "later rule wins" order):
 *
 *   1. Theme's `tokenColors` from JSON (baseTokenColors).
 *   2. Top-level `textMateRules` block (applies to all themes).
 *   3. `[<active-theme-label>].textMateRules` (per-theme overrides).
 *
 * VS Code's settings system has additional layering (user / workspace
 * / folder), but `getConfiguration('editor').get('tokenColorCustomizations')`
 * returns the merged effective value, so we don't have to re-implement
 * the scope precedence.
 */
function mergeCustomizations(args: {
    baseTokenColors: ThemeSetting[];
    tokenColorCustomizations: unknown;
    activeThemeLabel: string;
}): ThemeSetting[] {
    const merged = [...args.baseTokenColors];
    const cust = args.tokenColorCustomizations;
    if (!cust || typeof cust !== 'object') return merged;
    const obj = cust as Record<string, unknown>;

    function appendRulesFrom(node: unknown): void {
        if (!node || typeof node !== 'object') return;
        const rules = (node as { textMateRules?: unknown }).textMateRules;
        if (!Array.isArray(rules)) return;
        for (const r of rules) {
            const n = normalizeThemeSetting(r);
            if (n) merged.push(n);
        }
    }

    // 1. Global fallback block (the customizations object itself).
    appendRulesFrom(obj);

    // 2. Per-theme block keyed by `[<label>]`. The label comes from the
    //    contributed theme's `label` field; comparison is exact.
    const perTheme = obj[`[${args.activeThemeLabel}]`];
    appendRulesFrom(perTheme);

    return merged;
}

/**
 * Read the simple form of `semanticTokenColors` plus user customizations
 * into a `TokenRole → color` map. Only entries whose key is a bare
 * semantic-token type that maps to one of our roles are honored;
 * `type.modifier` selectors and modifier-style rules are ignored.
 *
 * VS Code's full semantic-token rule grammar is significantly more
 * complex (type+modifiers, language scoping, multiple selector
 * syntaxes). Implementing it properly would duplicate VS Code-internal
 * code and add a lot of surface for a Knit Preview viewer whose color
 * mapping is already 10-role coarse. The simple form covers the
 * majority of themes that explicitly opt-in for things like
 * `"function": "#abc"`.
 */
function readSemanticOverrides(args: {
    baseSemanticTokenColors: Record<string, unknown>;
    semanticTokenColorCustomizations: unknown;
    activeThemeLabel: string;
}): Partial<Record<TokenRole, string>> {
    const out: Partial<Record<TokenRole, string>> = {};

    // Base theme's semanticTokenColors first.
    overlay(out, args.baseSemanticTokenColors);

    const cust = args.semanticTokenColorCustomizations;
    if (cust && typeof cust === 'object') {
        const c = cust as Record<string, unknown>;
        // VS Code's setting wraps the rules under a `rules` key in the
        // customizations object, but legacy settings.json files may
        // place type-name keys at the top level too. Overlay BOTH so a
        // hand-edited mix (`{ rules: { variable: ... }, function: ... }`)
        // doesn't lose the top-level intent. Top-level first, `rules`
        // second so the canonical `rules` block wins on conflict.
        overlay(out, c);
        if (c.rules && typeof c.rules === 'object') {
            overlay(out, c.rules as Record<string, unknown>);
        }

        const perTheme = c[`[${args.activeThemeLabel}]`];
        if (perTheme && typeof perTheme === 'object') {
            const perThemeObj = perTheme as Record<string, unknown>;
            overlay(out, perThemeObj);
            if (perThemeObj.rules && typeof perThemeObj.rules === 'object') {
                overlay(out, perThemeObj.rules as Record<string, unknown>);
            }
        }
    }

    return out;

    function overlay(target: Partial<Record<TokenRole, string>>, source: Record<string, unknown>): void {
        for (const [key, value] of Object.entries(source)) {
            // Skip keys with dots (modifier selectors), `*` patterns,
            // language qualifiers, etc. — anything not a bare type.
            if (!/^[a-zA-Z][a-zA-Z0-9]*$/.test(key)) continue;
            const role = SEMANTIC_TYPE_TO_ROLE[key];
            if (!role) continue;
            const color = extractSemanticColor(value);
            if (color) target[role] = color;
        }
    }

    function extractSemanticColor(value: unknown): string | null {
        if (typeof value === 'string') {
            const lower = value.toLowerCase();
            return HEX_COLOR_RE.test(lower) ? lower : null;
        }
        if (value && typeof value === 'object') {
            const fg = (value as { foreground?: unknown }).foreground;
            if (typeof fg === 'string') {
                const lower = fg.toLowerCase();
                if (HEX_COLOR_RE.test(lower)) return lower;
            }
        }
        return null;
    }
}

// ---------------------------------------------------------------------
// vscode-textmate probing
// ---------------------------------------------------------------------

async function probeRoleColors(args: {
    registry: Pick<GrammarRegistry, 'extractWithTheme' | 'tokenizeLineForLanguage'>;
    tokenColors: readonly ThemeSetting[];
}): Promise<Partial<Record<TokenRole, string>>> {
    // The "no rule matched" foreground that vscode-textmate will use
    // for any token whose scope chain doesn't match any rule in
    // `args.tokenColors`. Two cases:
    //
    //   1. `tokenColors` has an empty-scope (or scope-undefined) rule.
    //      vscode-textmate treats that as the theme's defaults — its
    //      `foreground` becomes the no-match color. The LAST empty-
    //      scope rule wins, mirroring vscode-textmate's parseTheme.
    //   2. `tokenColors` has NO empty-scope rule. vscode-textmate
    //      hardcodes `#000000` as the foreground default.
    //
    // We compute this BEFORE setTheme so the inner callback can
    // filter votes that match it — preventing a role whose scope had
    // no specific theme rule from inheriting an invisible (`#000000`)
    // or undifferentiated (editor.foreground) color.
    const noMatchFg = effectiveDefaultForeground(args.tokenColors);

    // Pre-tokenize the corpus to capture scope chains. This is
    // theme-independent (vscode-textmate's `tokenizeLine` does not
    // consult the theme), so we can do it outside the
    // `extractWithTheme` callback's serialization window and avoid
    // double-tokenizing the same lines inside the lock.
    const corpus: Array<{ line: string; tokens: readonly ScopeToken[] }> = [];
    for (const line of SAMPLE_CORPUS) {
        const result = await args.registry.tokenizeLineForLanguage('r', line, null);
        if (!result) continue;
        corpus.push({ line, tokens: result.tokens });
    }
    if (corpus.length === 0) return {};

    return args.registry.extractWithTheme(args.tokenColors, async (api) => {
        // Per-role vote map: color → count.
        const votes: Record<TokenRole, Map<string, number>> = {
            keyword: new Map(),
            string: new Map(),
            number: new Map(),
            comment: new Map(),
            function: new Map(),
            type: new Map(),
            variable: new Map(),
            operator: new Map(),
            punctuation: new Map(),
            constant: new Map(),
        };

        for (const { line, tokens } of corpus) {
            const colorResult = await api.tokenizeLine2ForLanguage('r', line);
            if (!colorResult) continue;
            for (const tok of tokens) {
                const role = scopeToRole(tok.scopes);
                if (!role) continue;
                // `scopeToRole` walks innermost-first, so the `"` in
                // `"text"` (scope chain ending in
                // `punctuation.definition.string.*`) is classified as
                // punctuation. The theme's selector matcher, however,
                // resolves the color via the outer `string.*` scope —
                // so vscode-textmate paints that `"` the string color.
                // Voting that color into the punctuation role would
                // poison every other punctuation token (commas, parens)
                // with the string color. Skip these "wrong-role"
                // votes: a non-string/non-comment role whose chain
                // contains a string/comment scope is misclassified
                // for the purpose of role-color attribution.
                if (isWrongRoleForChain(role, tok.scopes)) continue;
                const color = colorAtOffset(colorResult.tokens, tok.startIndex, api.colorMap);
                if (color === null) continue;
                const normalized = color.toLowerCase();
                // Skip tokens whose color is the theme's no-rule
                // default — those add no information and we don't
                // want them outvoting a specific role color.
                if (normalized === noMatchFg) continue;
                const m = votes[role];
                m.set(normalized, (m.get(normalized) ?? 0) + 1);
            }
        }

        const out: Partial<Record<TokenRole, string>> = {};
        for (const role of Object.keys(votes) as TokenRole[]) {
            const voteMap = votes[role];
            if (voteMap.size === 0) continue;
            let bestColor: string | null = null;
            let bestCount = 0;
            for (const [color, count] of voteMap) {
                if (count > bestCount) {
                    bestColor = color;
                    bestCount = count;
                }
            }
            if (bestColor) out[role] = bestColor;
        }
        return out;
    });
}

/**
 * Synthesize an empty-scope default rule from the theme's
 * `colors.editor.foreground` / `colors.editor.background` when the
 * theme's `tokenColors` doesn't already contain one.
 *
 * Why: vscode-textmate's `resolveParsedThemeRules` uses the LAST
 * empty-scope rule as the theme's defaults, falling back to a
 * hardcoded `#000000` foreground when none exist. VS Code's editor
 * does NOT use that hardcoded value — it uses `editor.foreground`
 * from the workbench-level `colors` section. So a theme that styles
 * specific scopes but leaves a default rule off (e.g. Dark 2026)
 * shows unstyled tokens (punctuation, plain identifiers) in
 * `editor.foreground` in the editor — but vscode-textmate, called
 * directly with that theme's raw `tokenColors`, would return
 * `#000000` for them.
 *
 * Prepending a synthetic empty-scope rule with the editor.foreground/
 * background values makes our resolver mirror the editor's behavior.
 * If `tokenColors` already has a default rule, return unchanged — that
 * rule wins per "last empty-scope rule wins" anyway, and prepending
 * a synthetic earlier wouldn't change the outcome.
 */
function synthesizeDefaultRule(
    tokenColors: readonly ThemeSetting[],
    editorColors: Record<string, string>,
): ThemeSetting[] {
    // Skip synthesis only when there's already an empty-scope rule
    // that ACTUALLY provides a default foreground. A rule like
    // `{ scope: '', settings: { background: '#1e1e1e' } }` (no
    // foreground) does NOT defend against the #000000 fallback —
    // effectiveDefaultForeground will skip it because its `foreground`
    // is undefined and end up returning the #000000 sentinel. Treat
    // that case the same as "no default rule" and synthesize.
    const hasDefaultForeground = tokenColors.some((rule) => {
        if (!isEmptyScope(rule.scope)) return false;
        const fg = rule.settings.foreground;
        return typeof fg === 'string' && HEX_COLOR_RE.test(fg.toLowerCase());
    });
    if (hasDefaultForeground) {
        return [...tokenColors];
    }
    const fg = pickValidColor(editorColors['editor.foreground']);
    const bg = pickValidColor(editorColors['editor.background']);
    if (!fg && !bg) return [...tokenColors];
    const synthesized: ThemeSetting = {
        settings: {
            ...(fg ? { foreground: fg } : {}),
            ...(bg ? { background: bg } : {}),
        },
    };
    // Prepend so any explicit rules in tokenColors get a chance to
    // override; this synthetic rule only fires for tokens that match
    // nothing else AND (per the LAST-empty-scope-wins TextMate rule)
    // when no later empty-scope rule sets a foreground.
    return [synthesized, ...tokenColors];
}

/**
 * The foreground color vscode-textmate will assign to any token whose
 * scope chain matches none of `tokenColors`. Mirrors vscode-textmate's
 * `resolveParsedThemeRules`: rules with an empty scope (or with `scope`
 * omitted / set to the empty string) act as the theme's defaults, with
 * the LAST such rule winning; if none exist, the engine uses the
 * hardcoded `#000000`.
 */
function effectiveDefaultForeground(tokenColors: readonly ThemeSetting[]): string {
    let defaultFg = '#000000';
    for (const rule of tokenColors) {
        if (!isEmptyScope(rule.scope)) continue;
        const fg = rule.settings.foreground;
        if (typeof fg !== 'string') continue;
        const lower = fg.toLowerCase();
        if (HEX_COLOR_RE.test(lower)) defaultFg = lower;
    }
    return defaultFg;
}

/**
 * True when the token's scope chain contains a "dominating" string or
 * comment scope but `scopeToRole` classified it as something else (per
 * its innermost-first walk). vscode-textmate's selector matcher will
 * resolve such a token's color via the dominant scope, so attributing
 * that color to the token's innermost role is misleading.
 *
 * The asymmetry: tokens classified AS `string` or `comment` are
 * trustworthy by construction (their scope chain must start with the
 * matching prefix). It's the other roles — punctuation, keyword,
 * operator — that can get hijacked by an enclosing string/comment.
 */
function isWrongRoleForChain(role: TokenRole, scopes: readonly string[]): boolean {
    if (role === 'string' || role === 'comment') return false;
    for (const s of scopes) {
        // Match both `string.<sub>` (the common form) AND a bare
        // `string` / `comment` top-level scope. TextMate's naming
        // convention permits the bare form, and `scopeToRole`'s own
        // classification only fires on `string.` / `comment.` — so
        // a bare-scope token's innermost role is something else (e.g.
        // `punctuation.definition.string.begin`), and without this
        // equality check the string color would vote into the
        // punctuation role.
        if (s === 'string' || s === 'comment') return true;
        if (s.startsWith('string.') || s.startsWith('comment.')) return true;
    }
    return false;
}

function isEmptyScope(scope: string | string[] | undefined): boolean {
    if (scope === undefined) return true;
    if (typeof scope === 'string') return scope.trim().length === 0;
    if (Array.isArray(scope)) {
        return scope.length === 0 || scope.every((s) => typeof s !== 'string' || s.trim().length === 0);
    }
    return false;
}

/**
 * Locate the binary token covering `offset` and return its theme
 * foreground color, or `null` if the token uses the default
 * foreground (index 0, which means "no theme rule matched — paint
 * with the editor's default foreground").
 *
 * Binary layout per vscode-textmate: tokens is a flat Uint32Array
 * where index `2*i` is the token's startIndex and `2*i + 1` is the
 * encoded metadata.
 */
function colorAtOffset(
    tokens: Uint32Array,
    offset: number,
    colorMap: readonly string[],
): string | null {
    // Walk backwards from the end so a token whose range is
    // `[startIndex, lineLength)` for the last entry can still match
    // offsets at line-end.
    for (let i = tokens.length - 2; i >= 0; i -= 2) {
        const start = tokens[i];
        if (offset >= start) {
            const metadata = tokens[i + 1];
            const fgIdx = (metadata & FOREGROUND_MASK) >>> FOREGROUND_OFFSET;
            // Foreground index 0 is the registry's reserved sentinel
            // ("no color"). In practice vscode-textmate assigns at
            // least one default rule (whether from the theme's
            // empty-scope entry or its hardcoded fallback), so a
            // metadata value of 0 generally won't appear — but if it
            // does, treat as "no color" so the caller's fallback
            // kicks in.
            if (fgIdx === 0) return null;
            const color = colorMap[fgIdx];
            if (typeof color !== 'string') return null;
            const lower = color.toLowerCase();
            return HEX_COLOR_RE.test(lower) ? lower : null;
        }
    }
    return null;
}

// ---------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------

function pickValidColor(value: unknown): string | null {
    if (typeof value !== 'string') return null;
    const lower = value.toLowerCase();
    return HEX_COLOR_RE.test(lower) ? lower : null;
}
