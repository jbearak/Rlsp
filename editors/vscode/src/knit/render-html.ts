/**
 * Knit Preview rendering pipeline.
 *
 * The full flow (called from the post-knit code path in
 * `knit-commands.ts`):
 *
 *   1. R subprocess writes `<basename>.md` next to the source via
 *      `knitr::knit`.
 *   2. This module reads that `.md`.
 *   3. It calls `markdown.api.render` to convert to HTML using VS
 *      Code's full pipeline (KaTeX math, image rewriting, registered
 *      `markdown-it` plugins, etc.).
 *   4. It walks the HTML, finds every `<pre><code class="language-X">`
 *      block, replaces the body with grammar-aware GitHub-themed
 *      spans (via `code-highlighter.ts`). R blocks get Raven's LSP
 *      function-token overlay layered on top.
 *   5. It inlines KaTeX CSS + the GitHub light/dark stylesheets, then
 *      writes `<basename>.html`.
 *
 * The webview panel reads that `.html` and displays it in its iframe.
 * "Open in Browser" opens the same `.html` so the browser path gets
 * identical styling (theme picked via `prefers-color-scheme`).
 *
 * Everything in this module is pure-ish: side effects (reading the
 * `.md`, writing the `.html`, fetching the LSP semantic tokens, calling
 * VS Code's render API) are passed in as functions so unit tests can
 * exercise the full flow without spinning up VS Code.
 */

import type { GrammarRegistry } from './grammar-registry';
import {
    githubDark,
    githubLight,
    highlightCodeBlock,
    semanticOverlaysFromLspData,
    type GithubPalette,
} from './code-highlighter';
import { stripFrontmatter } from './yaml-frontmatter';

/** Per-language CSS class prefix `markdown-it` emits. */
const LANG_CLASS_PREFIX = 'language-';

/**
 * Hard-coded font fallbacks. Used when every upstream candidate
 * (raven-knit setting, VS Code default) is empty or rejected by the
 * sanitizer.
 *
 * `MONO_HARDCODED_FALLBACK` is the exact string `baseStyles()` shipped
 * before user-configurable fonts existed, so a user who removes their
 * setting (or whose VS Code defaults somehow fail the sanitizer) lands
 * back on the historical default rather than something new.
 */
const TEXT_HARDCODED_FALLBACK =
    '-apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif';
const MONO_HARDCODED_FALLBACK =
    'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace';

/**
 * CSS generic-family keywords. If a resolved font-family string already
 * ends with one of these, we don't append our own terminator — the
 * user's list already has a final fallback.
 *
 * Comparison is case-insensitive and ignores trailing whitespace. The
 * list mirrors the CSS Fonts Module Level 4 generic family keywords; we
 * include `emoji`, `math`, `fangsong` for completeness even though
 * they're rarely used as a list terminator.
 */
const GENERIC_FAMILY_KEYWORDS = new Set([
    'monospace',
    'sans-serif',
    'serif',
    'system-ui',
    'ui-monospace',
    'ui-sans-serif',
    'ui-serif',
    'ui-rounded',
    'cursive',
    'fantasy',
    'emoji',
    'math',
    'fangsong',
]);

/**
 * CSS-wide value keywords. We REJECT these as the entire user font
 * value because they don't behave usefully in the knit-output iframe:
 * the iframe's <body> has no meaningful author-controlled parent for
 * `inherit` to pull from, and `initial` / `unset` / `revert` would
 * undo the very styling the user is trying to configure. Treating
 * them as banned shapes makes the sanitizer reject them up front so
 * the user falls through to a working fallback rather than seeing
 * UA-default Times Roman.
 */
const CSS_WIDE_KEYWORDS = new Set([
    'inherit',
    'initial',
    'unset',
    'revert',
    'revert-layer',
]);

/**
 * Banned characters in a user-supplied font-family value. The regex
 * forms the structural trust boundary between settings input and the
 * `<style>` block this module emits.
 *
 * Coverage rationale (each char neutralises a specific CSS attack
 * surface — see `sanitizeFontFamily`'s doc comment for the threat
 * model):
 *
 *   `; { } < > \\`       — break out of the declaration, the rule
 *                          block, or the `<style>` element.
 *   `\n \r`              — newline forms in CSS Syntax L3 §3.3 — any
 *                          would terminate a string token or property.
 *   `\t \f \v`           — whitespace that CSS preprocesses or
 *                          tokenises in ways that split unquoted
 *                          family names (`\f` is a CSS newline; `\t`
 *                          and `\v` whitespace inside an identifier
 *                          breaks it).
 *   `\0`                 — CSS Syntax L3 §3.3 replaces NUL with U+FFFD;
 *                          rejecting it up-front keeps the sanitizer's
 *                          textual ban-set stable across that rewrite.
 *
 * NOT in the class: `(` and `)`. They are dangerous OUTSIDE a quoted
 * family name (an unquoted `Foo(` opens a function-token whose
 * consumption ignores `}` boundaries and can corrupt the rest of the
 * stylesheet), but they appear LEGITIMATELY in quoted real-world font
 * names like `"Aptos (Body)"`. `hasBareParens` enforces the
 * inside-quotes-only rule below.
 */
const BANNED_CHAR_RE = /[;{}<>\\\n\r\t\f\v\0]/;

/**
 * Render a post-knit `.md` source string into the final HTML body
 * we'll show in the panel and write to disk for "Open in Browser".
 *
 * The output is a self-contained HTML document: `<!doctype html>` +
 * inlined GitHub + KaTeX stylesheets + the rendered body. No external
 * scripts; the iframe sandbox in the knit-output panel is
 * `allow-same-origin` only (no `allow-scripts`), so we cannot rely on
 * client-side JS to do anything.
 */
export async function renderKnitHtml(args: {
    /** Post-knit markdown source. */
    markdownSource: string;

    /**
     * Renders markdown -> HTML using VS Code's pipeline. The default
     * production wiring calls `vscode.commands.executeCommand(
     * 'markdown.api.render', source)`. Tests pass a fake.
     */
    renderMarkdown: (source: string) => Promise<string>;

    /**
     * Tokenizer registry used by `highlightCodeBlock`. Production
     * callers pass the registry built from `createGrammarRegistry`.
     */
    registry: GrammarRegistry;

    /**
     * Fetches Raven's LSP `function` semantic tokens for raw R source
     * text via the `raven/semanticTokensForRString` custom request.
     * Returns the LSP-encoded `data: number[]` array, decoded with
     * `semanticOverlaysFromLspData` inside this module. Tests pass a
     * fake; if undefined, R code blocks get vscode-textmate-only
     * highlighting (no function-name overlay).
     */
    fetchRSemanticTokens?: (text: string) => Promise<ArrayLike<number>>;

    /**
     * The KaTeX CSS to inline. Production callers read the file
     * shipped by VS Code's `vscode.markdown-math` extension; tests
     * pass an empty string.
     */
    katexCss?: string;

    /**
     * Sets the data-theme attribute on the wrapper so the inlined
     * GitHub palettes can switch via either `prefers-color-scheme`
     * (browser) or the parent webview's body class (in-VS Code).
     * `null` produces a stylesheet that defaults to light and flips
     * to dark via `@media (prefers-color-scheme: dark)`; non-null
     * paints whichever palette the panel is currently themed for.
     */
    themeClasses?: string | null;

    /**
     * Already-resolved font-family strings for body and monospace.
     * Production callers pass the result of `resolveFontFamilies` —
     * sanitized, with a generic-family terminator appended. Tests
     * may omit this; the renderer falls back to the hardcoded
     * defaults that match the historical behavior.
     */
    fonts?: ResolvedFonts;
}): Promise<string> {
    // Strip the YAML frontmatter before invoking the renderer so the
    // VS Code markdown pipeline never emits its `<table class="frontmatter">`
    // for the preview. The on-disk .md (which Pandoc export reads) is
    // untouched — the strip only mutates the in-memory string passed to
    // `args.renderMarkdown`. See
    // docs/superpowers/specs/2026-05-25-knit-preview-yaml-table-design.md.
    const html = await args.renderMarkdown(stripFrontmatter(args.markdownSource));
    const rewritten = await rewriteCodeBlocks(html, args);
    return assembleDocument(rewritten, args);
}

/**
 * Walk the rendered HTML for `<pre><code class="...language-X...">`
 * blocks (markdown-it's emitted shape), replace each block's body
 * with our highlighted version, and leave everything else
 * (paragraphs, math spans, raw HTML pass-through) alone.
 */
async function rewriteCodeBlocks(
    html: string,
    args: {
        registry: GrammarRegistry;
        fetchRSemanticTokens?: (text: string) => Promise<ArrayLike<number>>;
        themeClasses?: string | null;
    },
): Promise<string> {
    const out: string[] = [];
    let cursor = 0;
    // Loose regex: VS Code's pipeline emits attributes in different
    // orders depending on plugins (`<code data-line="N" class="...">`
    // vs `<code class="..." data-line="N">`), so we match
    // case-insensitively and scan from cursor each iteration. The
    // language attribute can be one of several classes — we extract
    // it from the class attribute below rather than constraining its
    // position in the regex.
    const blockRe = /<pre\b[^>]*>\s*<code\b([^>]*)>([\s\S]*?)<\/code>\s*<\/pre>/gi;
    blockRe.lastIndex = 0;
    let match: RegExpExecArray | null;
    while ((match = blockRe.exec(html)) !== null) {
        const matchStart = match.index;
        const matchEnd = matchStart + match[0].length;
        const codeAttrs = match[1];
        const innerEncoded = match[2];

        out.push(html.slice(cursor, matchStart));

        const languageId = extractLanguageId(codeAttrs);
        if (!languageId) {
            // No language tag — leave the original block intact so the
            // user's plain ``` blocks keep their inherited styling.
            out.push(match[0]);
            cursor = matchEnd;
            continue;
        }

        const rawSource = decodeCodeBlock(innerEncoded);
        let overlays: ReturnType<typeof semanticOverlaysFromLspData> = [];
        if (languageId === 'r' && args.fetchRSemanticTokens) {
            try {
                const data = await args.fetchRSemanticTokens(rawSource);
                overlays = semanticOverlaysFromLspData(data, rawSource);
            } catch (err) {
                // Fall back to grammar-only highlighting.
                console.error(
                    '[raven-knit] fetchRSemanticTokens failed: ' +
                        (err instanceof Error ? err.message : String(err)),
                );
            }
        }
        const highlighted = await highlightCodeBlock({
            source: rawSource,
            languageId,
            registry: args.registry,
            overlays,
        });
        // Marker class `raven-knit-code` scopes the panel chrome
        // (border, padding, background) to highlighted code blocks
        // only — output blocks (no `language-X` class) are left
        // untagged and render as bare monospace, so prose readers can
        // distinguish input from output the same way Quarto's preview
        // does. The base stylesheet in `baseStyles()` keys off this
        // class, as does the theme overlay in
        // `knit-output.ts:applyTheme`. Keep all three in lockstep.
        out.push(
            `<pre class="raven-knit-code"><code class="${LANG_CLASS_PREFIX}${escapeAttr(languageId)}">` +
                highlighted +
                `</code></pre>`,
        );
        cursor = matchEnd;
    }
    out.push(html.slice(cursor));
    return out.join('');
}

/**
 * Pull the language ID out of a `<code class="…language-X…">` attribute
 * list. VS Code's pipeline (highlight.js mode) emits the class
 * `language-r`; markdown-it's default also emits `language-r`.
 * Returns the language ID lowercased, or null when none is declared.
 */
export function extractLanguageId(codeAttrs: string): string | null {
    const classMatch = codeAttrs.match(/\bclass\s*=\s*"([^"]*)"/i)
        ?? codeAttrs.match(/\bclass\s*=\s*'([^']*)'/i);
    if (!classMatch) return null;
    const classes = classMatch[1].split(/\s+/);
    for (const c of classes) {
        if (c.startsWith(LANG_CLASS_PREFIX)) {
            const lang = c.slice(LANG_CLASS_PREFIX.length).trim().toLowerCase();
            if (lang.length > 0) return lang;
        }
    }
    return null;
}

/**
 * Decode HTML-escaped code-block body back to its raw source.
 *
 * Two layers to reverse:
 *
 *   1. Inline highlighter markup. VS Code's `markdown.api.render`
 *      runs each code block through markdown-it's `highlight` hook,
 *      which on the preview pipeline pre-tokenizes via highlight.js
 *      and emits `<span class="hljs-...">…</span>` wrappers inside
 *      the `<code>`. Those tags are part of the markup, not of the
 *      source. We strip them first so the inner text reads as a
 *      verbatim copy of what the user wrote.
 *
 *   2. The five HTML metacharacters that the renderer escapes
 *      (`&`, `<`, `>`, `"`, `'`) need to be reversed before we feed
 *      the text to vscode-textmate.
 *
 * Stripping tags BEFORE entity-decoding is essential: in the
 * rendered inner content every literal `<` from the source code has
 * already been escaped to `&lt;`, so any remaining `<` is part of a
 * highlighter tag. Decoding first would conflate the two.
 *
 * Entity-reversal order also matters: `&amp;` must be reversed LAST
 * so a literal `&amp;lt;` (the escape sequence for the text `&lt;`)
 * round-trips correctly.
 *
 * The tag-strip runs in a fixed-point loop. CodeQL's "incomplete
 * multi-character sanitization" rule (alert /security/code-scanning/17
 * against this function) flags any single-pass `<[^>]*>` strip:
 * adversarial markup like `<scr<tag>ipt>` survives one pass as
 * `script>` because removing the inner tag re-creates an opening
 * sequence. Looping until the string stabilises closes that gap.
 *
 * Belt-and-braces: the decoded source is never emitted as HTML
 * directly — it's tokenized by vscode-textmate and every slice is
 * re-escaped via `escapeHtml` before reaching the rendered output —
 * so the practical injection surface is already nil. We still
 * defend at this layer because (a) the alert is real against the
 * abstract function, and (b) future callers of `decodeCodeBlock`
 * should not have to rely on the downstream escape to be safe.
 */
export function decodeCodeBlock(encoded: string): string {
    let prev: string;
    let stripped = encoded;
    do {
        prev = stripped;
        stripped = stripped.replace(/<[^>]*>/g, '');
    } while (stripped !== prev);
    return stripped
        .replace(/&lt;/gi, '<')
        .replace(/&gt;/gi, '>')
        .replace(/&quot;/gi, '"')
        .replace(/&#39;/gi, "'")
        .replace(/&amp;/gi, '&');
}

/** Escape a value for use inside an HTML attribute. */
function escapeAttr(value: string): string {
    return value
        .replace(/&/g, '&amp;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');
}

/**
 * Build the final HTML document around the rewritten body. Two CSS
 * paths are emitted:
 *
 *   - GitHub light/dark palettes scoped to either
 *     `prefers-color-scheme` (when `themeClasses` is null — i.e. the
 *     standalone file we write for "Open in Browser") or to the
 *     panel's body class (when running inside VS Code).
 *   - KaTeX CSS, inlined verbatim so math renders without further
 *     resource fetches (the iframe-sandbox blocks scripts AND
 *     external link rel=stylesheet on most platforms).
 */
function assembleDocument(
    body: string,
    args: { katexCss?: string; themeClasses?: string | null; fonts?: ResolvedFonts },
): string {
    const css = composeStylesheet(args.themeClasses ?? null, args.fonts) +
        (args.katexCss ? `\n${args.katexCss}\n` : '');
    return `<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>${css}</style>
</head>
<body>
${body}
</body>
</html>`;
}

/**
 * Build the GitHub light/dark stylesheet. When `themeClasses` is null
 * we emit both palettes with the dark one scoped to
 * `prefers-color-scheme: dark`; the rendered file works in both light
 * and dark system themes when opened standalone in a browser. When
 * `themeClasses` is non-null we paint whichever palette matches the
 * caller's theme — the in-VS-Code panel passes the body class so the
 * code-block colors match the active editor theme variant.
 */
export function composeStylesheet(
    themeClasses: string | null,
    fonts?: ResolvedFonts,
): string {
    const isLight = themeClasses !== null && /\bvscode-(light|high-contrast-light)\b/.test(themeClasses);

    // Always include both palettes as data so we can swap them on
    // demand. The selector for "is this the dark variant" varies
    // (`prefers-color-scheme` for the standalone case, body-class for
    // the in-VS-Code panel). Below we set the active variant up
    // front and then add a media-query swap when running standalone.
    const lightVars = paletteAsCssVars(githubLight);
    const darkVars = paletteAsCssVars(githubDark);
    // Font vars are emitted alongside the palette in the same outer
    // `:root { }`. Unlike the palette they do NOT vary by variant, so
    // the variant-conditional inner-:root (the
    // `prefers-color-scheme: dark` media query below, and the
    // body-class branch for the in-VS-Code panel) keeps shipping
    // colors only. Callers without `fonts` get the hardcoded
    // historical defaults routed through `resolveFontFamilies` so the
    // terminator-append invariant lives in one place — changing a
    // hardcoded constant cannot silently strip the generic-family
    // fallback from the emitted CSS.
    const resolvedFonts: ResolvedFonts = fonts ?? resolveFontFamilies('', '', '', '');
    const fontVars = fontsAsCssVars(resolvedFonts);

    if (themeClasses === null) {
        // Standalone (browser) — start light, swap on prefers-color-scheme: dark.
        return `
:root {
  color-scheme: light dark;
  ${lightVars}
  ${fontVars}
}
@media (prefers-color-scheme: dark) {
  :root {
    ${darkVars}
  }
}
${baseStyles()}
`.trim();
    }

    return `
:root {
  color-scheme: ${isLight ? 'light' : 'dark'};
  ${isLight ? lightVars : darkVars}
  ${fontVars}
}
${baseStyles()}
`.trim();
}

/**
 * Resolved font-family strings ready to drop into CSS values. Both
 * fields are sanitized and end with a generic-family terminator; the
 * renderer treats them as trusted input.
 */
export interface ResolvedFonts {
    /** Body / prose font-family. */
    text: string;
    /** Monospace font-family for code chunks and output blocks. */
    mono: string;
}

/**
 * Sanitize a user-supplied font-family string for inclusion in a CSS
 * property value.
 *
 * Font-family is a free-form CSS value: it accepts comma-separated
 * lists with quoted names that may include spaces (e.g.
 * `'JetBrains Mono', "Source Sans Pro", monospace`). We can't validate
 * the grammar usefully — too many shapes — but we can reject the
 * specific characters that would let the string break out of the CSS
 * value, escape the `<style>` block, smuggle a comment that survives
 * into the rendered stylesheet, OR survive into the rendered CSS as a
 * value that the browser will silently treat as invalid (causing the
 * font-family property to drop via IACVT and revert to the UA default).
 *
 * Banned shapes:
 *   - Length > 500 chars (DoS / accidental paste of unrelated content).
 *   - Banned characters per `BANNED_CHAR_RE` (see that constant).
 *   - CSS comment sequences `/`+`*` / `*`+`/`.
 *   - Unbalanced quotes — a stray `"` or `'` would open a CSS string
 *     that runs until the next quote of the same kind or until EOF /
 *     newline, swallowing adjacent declarations as part of the
 *     bad-string recovery.
 *   - Bare parens — any `(` or `)` appearing OUTSIDE a quoted family
 *     name. Bare `Foo(` opens a CSS function-token whose consumption
 *     ignores `}` boundaries and can corrupt the rest of the
 *     stylesheet. Parens inside `"…"` or `'…'` are fine (CSS treats
 *     them as part of the string's content), so a setting of
 *     `"Aptos (Body)", sans-serif` is accepted.
 *   - Trailing or consecutive commas — `Foo,` becomes `Foo,, sans-serif`
 *     after our terminator is appended; var() substitution then makes
 *     the font-family declaration invalid and the property is dropped
 *     at IACVT. Reject empty top-level entries up front so the user
 *     falls through to a working fallback.
 *   - The CSS-wide keywords (`inherit` / `initial` / `unset` / `revert`
 *     / `revert-layer`) as the entire value. They don't behave usefully
 *     in the knit-output iframe and should fall through to a real font.
 *
 * Returns the trimmed input on success, or `null` if the input is
 * rejected. A `null` return is interpreted as "try the next layer of
 * the fallback chain" by `resolveSlot`.
 */
export function sanitizeFontFamily(input: string): string | null {
    if (typeof input !== 'string') return null;
    const trimmed = input.trim();
    if (trimmed.length === 0) return null;
    if (trimmed.length > 500) return null;
    if (BANNED_CHAR_RE.test(trimmed)) return null;
    // CSS comment sequences. A literal `/*` opens a comment that
    // extends until `*\/`, which would let an attacker comment out
    // every property between the font-family and the next legitimate
    // closing brace.
    if (trimmed.includes('/*') || trimmed.includes('*/')) return null;
    if (!hasBalancedQuotes(trimmed)) return null;
    if (hasBareParens(trimmed)) return null;
    if (hasEmptyTopLevelSegment(trimmed)) return null;
    if (CSS_WIDE_KEYWORDS.has(trimmed.toLowerCase())) return null;
    return trimmed;
}

/**
 * Returns `true` if any `(` or `)` appears OUTSIDE a quoted family
 * name. A bare `(` would open a CSS function-token whose consumption
 * ignores `}` boundaries and can corrupt the rest of the stylesheet;
 * a bare `)` is benign on its own but the symmetric ban keeps the
 * rule simple and matches user intent (paren only meaningful inside
 * the name).
 *
 * Parens INSIDE `"…"` or `'…'` are allowed so real-world font names
 * like `"Aptos (Body)"` can be configured. CSS treats those as
 * string content, not as function-token openers.
 */
function hasBareParens(value: string): boolean {
    let quote: '"' | "'" | null = null;
    for (let i = 0; i < value.length; i++) {
        const ch = value[i];
        if (quote) {
            if (ch === quote) quote = null;
            continue;
        }
        if (ch === '"' || ch === "'") {
            quote = ch as '"' | "'";
            continue;
        }
        if (ch === '(' || ch === ')') return true;
    }
    return false;
}

/**
 * Walk a font-family list and verify every `"` and `'` has a matching
 * close. Mixed quote types are fine (`'…' "…"`); each opens its own
 * scope.
 *
 * Returns `false` if the value ends with an open quote — that would
 * survive into the emitted CSS as an unterminated string token,
 * triggering bad-string-token recovery that swallows the sibling
 * declaration on the next line.
 */
function hasBalancedQuotes(value: string): boolean {
    let quote: '"' | "'" | null = null;
    for (let i = 0; i < value.length; i++) {
        const ch = value[i];
        if (quote) {
            if (ch === quote) quote = null;
            continue;
        }
        if (ch === '"' || ch === "'") quote = ch as '"' | "'";
    }
    return quote === null;
}

/**
 * Returns `true` if any top-level entry in the comma-separated list is
 * empty (or a degenerate empty-quoted name like `""` / `''`) after
 * trimming. Trailing comma, leading comma, and consecutive commas all
 * produce an empty entry; once `appendGenericTerminator` appends
 * `, sans-serif` the resulting value becomes invalid font-family syntax
 * (`Foo,, sans-serif`) and the browser drops the declaration via
 * IACVT. Empty quoted entries (`""`) are also rejected because CSS
 * treats them as a custom family with the empty string for a name,
 * which no font matches — the user's intent was almost certainly
 * something else.
 *
 * Top-level means outside `"…"` or `'…'` quoted family names —
 * `"Comma, Foundry"` is a single entry.
 */
function hasEmptyTopLevelSegment(value: string): boolean {
    let quote: '"' | "'" | null = null;
    let segmentStart = 0;
    for (let i = 0; i <= value.length; i++) {
        const ch = i < value.length ? value[i] : ',';
        if (quote) {
            if (ch === quote) quote = null;
            continue;
        }
        if (ch === '"' || ch === "'") {
            quote = ch as '"' | "'";
            continue;
        }
        if (ch === ',' || i === value.length) {
            const segment = value.slice(segmentStart, i).trim();
            if (segment.length === 0) return true;
            // Empty quoted-string family name (e.g. `""` or `''`) —
            // CSS would treat this as a custom family with no name,
            // which is never what the user meant.
            if (segment === '""' || segment === "''") return true;
            segmentStart = i + 1;
        }
    }
    return false;
}

/**
 * Walk the font fallback chain and return resolved, sanitized,
 * terminator-guaranteed strings for body and monospace.
 *
 * Chain (per slot, first non-null wins):
 *   1. User setting (`raven.knit.fontFamily` / `monospaceFontFamily`).
 *   2. VS Code fallback (`markdown.preview.fontFamily` /
 *      `editor.fontFamily`) — VS Code resolves these to OS-specific
 *      defaults via `getConfiguration(...).get(...)` even when the
 *      user has not set them.
 *   3. Hard-coded fallback (only fires if the sanitizer rejects the
 *      VS Code value, since `get()` itself never returns empty for
 *      these well-known settings).
 *
 * After picking the winner, a generic-family terminator is appended
 * (`, monospace` for mono, `, sans-serif` for text) unless the resolved
 * string already ends with one. This guarantees the baked `.html`
 * degrades gracefully when opened in a browser on a machine that
 * doesn't have the user's configured fonts installed — the browser
 * falls through to a generic family rather than reverting to its own
 * Times default.
 */
export function resolveFontFamilies(
    textRaw: string,
    monoRaw: string,
    textFallback: string,
    monoFallback: string,
): ResolvedFonts {
    return {
        text: resolveSlot(textRaw, textFallback, TEXT_HARDCODED_FALLBACK, 'sans-serif'),
        mono: resolveSlot(monoRaw, monoFallback, MONO_HARDCODED_FALLBACK, 'monospace'),
    };
}

function resolveSlot(
    primary: string,
    fallback: string,
    hardcoded: string,
    terminator: string,
): string {
    const picked =
        sanitizeFontFamily(primary)
        ?? sanitizeFontFamily(fallback)
        ?? hardcoded;
    return appendGenericTerminator(picked, terminator);
}

function appendGenericTerminator(value: string, terminator: string): string {
    // Split on the LAST top-level comma so quoted names containing
    // commas inside their quoted forms (unusual but legal: e.g.
    // `"Comma, Foundry"`) don't confuse the check. The CSS grammar
    // uses comma at the top level as the family separator; commas
    // inside quoted family names are part of the name.
    const lastComma = lastTopLevelComma(value);
    const lastEntryRaw = lastComma < 0 ? value : value.slice(lastComma + 1);
    const lastEntry = lastEntryRaw.trim().toLowerCase();
    // Per CSS spec, generic family keywords (`monospace`, `serif`, …)
    // are bare identifiers. A QUOTED `"monospace"` is a custom family
    // name, NOT the generic keyword — so we treat any quoted last
    // entry as "not a terminator" and append our own generic, which
    // is what the browser ultimately needs to find a real fallback.
    if (lastEntry.startsWith('"') || lastEntry.startsWith("'")) {
        return `${value}, ${terminator}`;
    }
    if (GENERIC_FAMILY_KEYWORDS.has(lastEntry)) return value;
    return `${value}, ${terminator}`;
}

/**
 * Index of the last top-level comma in a font-family list, or -1 if
 * none. "Top-level" means outside any `"..."` or `'...'` quoted family
 * name. Returns -1 for inputs with no comma.
 */
function lastTopLevelComma(value: string): number {
    let quote: '"' | "'" | null = null;
    let last = -1;
    for (let i = 0; i < value.length; i++) {
        const ch = value[i];
        if (quote) {
            if (ch === quote) quote = null;
            continue;
        }
        if (ch === '"' || ch === "'") {
            quote = ch as '"' | "'";
            continue;
        }
        if (ch === ',') last = i;
    }
    return last;
}

/**
 * Emit the `--raven-font-text` / `--raven-font-mono` declarations as
 * a single chunk for splicing into the outer `:root { }` block — same
 * shape as `paletteAsCssVars`, joined with the trailing newline +
 * indent that the inline-template assembly uses.
 *
 * Inputs are trusted: callers are expected to have run them through
 * `resolveFontFamilies`, which sanitizes every candidate.
 */
function fontsAsCssVars(fonts: ResolvedFonts): string {
    return [
        `--raven-font-text: ${fonts.text};`,
        `--raven-font-mono: ${fonts.mono};`,
    ].join('\n  ');
}

function paletteAsCssVars(palette: GithubPalette): string {
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
    ].join('\n  ');
}

function baseStyles(): string {
    // Only `pre.raven-knit-code` (input chunks) gets the panel chrome
    // (border, padding, rounded corners, background). Output `<pre>`
    // blocks — emitted by knitr without a `language-X` class and
    // therefore left untagged by `rewriteCodeBlocks` — render as bare
    // monospace text so readers can tell input from output at a glance,
    // matching Quarto's preview style.
    //
    // `overflow-x: auto` lives on the bare `pre` selector so wide
    // output (long printed lines, wide data frames) still scrolls
    // within the block rather than widening the document.
    return `
body {
  background: var(--raven-bg);
  color: var(--raven-fg);
  font-family: var(--raven-font-text);
}
pre {
  overflow-x: auto;
}
code {
  font-family: var(--raven-font-mono);
  color: var(--raven-fg);
}
/*
 * Keep ordinary raster images (and any other bare img, e.g. a PNG
 * from knitr::include_graphics()) inside the content width instead of
 * overflowing the preview pane (issue #629). We use max-width rather
 * than width so a naturally small image is never enlarged, and
 * height:auto preserves the aspect ratio even when an HTML width or
 * height attribute is present. The underlying pixels are untouched, so
 * a high-DPI figure keeps its full resolution -- only its display size
 * is capped. Extracted SVG plots are handled separately by the
 * .raven-knit-plot-host rules below and are unaffected.
 */
img {
  max-width: 100%;
  height: auto;
}
.raven-knit-plot-host {
  display: inline-flex;
  max-width: 100%;
  vertical-align: middle;
}
.raven-knit-plot-host svg {
  max-width: 100%;
  height: auto;
}
pre.raven-knit-code,
pre.raven-knit-code code {
  background: var(--raven-bg);
  color: var(--raven-fg);
}
pre.raven-knit-code {
  padding: 0.75rem 1rem;
  border-radius: 6px;
  border: 1px solid color-mix(in srgb, var(--raven-fg) 15%, transparent);
}
`.trim();
}
