import { describe, test, expect } from 'bun:test';
import {
    composeStylesheet,
    decodeCodeBlock,
    extractLanguageId,
    renderKnitHtml,
    resolveFontFamilies,
    sanitizeFontFamily,
} from '../../editors/vscode/src/knit/render-html';
import { githubDark, githubLight } from '../../editors/vscode/src/knit/code-highlighter';
import type {
    GrammarRegistry,
    ScopeToken,
} from '../../editors/vscode/src/knit/grammar-registry';

function fakeRegistry(
    tokenizers: Record<string, (line: string) => ScopeToken[]>,
): GrammarRegistry {
    return {
        async tokenizeLineForLanguage(languageId, line) {
            const tokenizer = tokenizers[languageId.toLowerCase()];
            if (!tokenizer) return null;
            return { tokens: tokenizer(line), ruleStack: null };
        },
        scopeNameFor(languageId) {
            return tokenizers[languageId.toLowerCase()] ? `source.${languageId}` : null;
        },
        async primeForLanguage(languageId) {
            return Boolean(tokenizers[languageId.toLowerCase()]);
        },
        async extractWithTheme(_themeSettings, _inner) {
            // The render-html pipeline only consumes the raw-scope path
            // (`tokenizeLineForLanguage`); the theme-aware extraction
            // is the panel/palette resolver's responsibility. Throw so
            // any test that wires this fake through a code path that
            // does need theme extraction fails loudly rather than
            // silently returning bogus values.
            throw new Error('fakeRegistry.extractWithTheme is not implemented');
        },
    };
}

describe('extractLanguageId', () => {
    test('finds language-X in a single class attribute', () => {
        expect(extractLanguageId(' data-line="2" class="language-r code-line" dir="auto"')).toBe('r');
    });

    test('matches single-quoted attributes too', () => {
        expect(extractLanguageId(" class='language-python'")).toBe('python');
    });

    test('lowercases the language id', () => {
        expect(extractLanguageId(' class="language-R"')).toBe('r');
    });

    test('returns null when no language- class is present', () => {
        expect(extractLanguageId(' class="code-line"')).toBeNull();
        expect(extractLanguageId('')).toBeNull();
    });
});

describe('decodeCodeBlock', () => {
    test('reverses the standard HTML escapes', () => {
        expect(decodeCodeBlock('a &amp; b &lt;= c &gt; d &quot;e&quot; f&#39;g'))
            .toBe(`a & b <= c > d "e" f'g`);
    });

    test('double-escaped ampersands round-trip correctly', () => {
        // `&amp;lt;` should decode to `&lt;`, not `<`.
        expect(decodeCodeBlock('&amp;lt;')).toBe('&lt;');
    });

    test('strips inline highlight.js wrapper spans before decoding entities', () => {
        // VS Code's `markdown.api.render` pre-tokenizes code blocks
        // via markdown-it's `highlight` hook (highlight.js by
        // default), so the `<code>` body we receive contains nested
        // `<span class="hljs-...">…</span>` wrappers. Those tags are
        // markup, not source — they must be stripped before we
        // hand the text to vscode-textmate or the grammar will
        // tokenize the literal HTML markup as code.
        const encoded =
            'library<span class="hljs-punctuation">(</span>' +
            'ggplot2<span class="hljs-punctuation">)</span>';
        expect(decodeCodeBlock(encoded)).toBe('library(ggplot2)');
    });

    test('preserves user-source `<` (entity-escaped) after stripping markup', () => {
        // Source `x <- 1` arrives as `x <span class="hljs-keyword">&lt;-</span> 1`.
        // After stripping spans + decoding entities we should be left
        // with `x <- 1` exactly.
        const encoded = 'x <span class="hljs-keyword">&lt;-</span> 1';
        expect(decodeCodeBlock(encoded)).toBe('x <- 1');
    });

    test('does not eat literal `&lt;span&gt;` text inside the source', () => {
        // Pathological: the source itself literally contains `<span>`.
        // The renderer escapes it to `&lt;span&gt;`. After our pass
        // there are no real tags to strip, and entity-decoding
        // restores the literal text.
        expect(decodeCodeBlock('foo &lt;span&gt; bar')).toBe('foo <span> bar');
    });

    test('does not leave any `<...>` tag shape behind for nested or adversarial markup', () => {
        // CodeQL alert 17 ("Incomplete multi-character sanitization")
        // flagged the single-pass `replace(/<[^>]*>/g, '')` as
        // potentially leaving residual tag-shape substrings on
        // adversarial input. Our greedy + global regex already
        // strips everything in one pass for the inputs we've
        // observed, but `decodeCodeBlock` now loops the strip to a
        // fixed point so the invariant survives future regex
        // tweaks. The contract locked down here is the survival
        // invariant — no remaining `<[^>]*>` substring — not the
        // exact textual remnant.
        for (const input of [
            '<scr<x>ipt>alert(1)</scr<x>ipt>',
            '<a<b<c<d>>>>',
            '<<>>',
            'a<<b>>c',
        ]) {
            expect(decodeCodeBlock(input)).not.toMatch(/<[^>]*>/);
        }
    });
});

describe('composeStylesheet', () => {
    test('null themeClasses emits both palettes via prefers-color-scheme', () => {
        const css = composeStylesheet(null);
        expect(css).toContain('color-scheme: light dark');
        expect(css).toContain('prefers-color-scheme: dark');
        expect(css).toContain(githubLight.background);
        expect(css).toContain(githubDark.background);
    });

    test('constrains ordinary images to the content width (issue #629)', () => {
        // A bare <img> (e.g. a large PNG from include_graphics) must be
        // capped to the content width without being enlarged or
        // distorted. The same stylesheet feeds the webview iframe and
        // "Open in Browser", so this one assertion covers both.
        const css = composeStylesheet(null);
        expect(css).toMatch(/\bimg\s*\{[^}]*max-width:\s*100%/);
        expect(css).toMatch(/\bimg\s*\{[^}]*height:\s*auto/);
        // The extracted-SVG plot rules must remain intact and separate.
        expect(css).toContain('.raven-knit-plot-host svg {');
    });

    test('vscode-light produces only the light palette and no media query', () => {
        const css = composeStylesheet('vscode-light');
        expect(css).toContain('color-scheme: light');
        expect(css).not.toContain('prefers-color-scheme');
        expect(css).toContain(githubLight.background);
        expect(css).not.toContain(githubDark.background);
    });

    test('vscode-dark produces only the dark palette and no media query', () => {
        const css = composeStylesheet('vscode-dark');
        expect(css).toContain('color-scheme: dark');
        expect(css).not.toContain('prefers-color-scheme');
        expect(css).toContain(githubDark.background);
        expect(css).not.toContain(githubLight.background);
    });

    test.each(['vscode-light', 'vscode-dark'])(
        'themed branch (themeClasses=%p) still emits font vars',
        (themeClasses) => {
            // Today's production call site hardcodes themeClasses=null
            // so the themed branch's `${fontVars}` interpolation is
            // dead-by-call-site; pin coverage so a future themed-knits
            // PR doesn't silently regress fonts.
            const css = composeStylesheet(themeClasses, {
                text: 'Georgia, serif',
                mono: 'Menlo, monospace',
            });
            expect(css).toContain('--raven-font-text: Georgia, serif;');
            expect(css).toContain('--raven-font-mono: Menlo, monospace;');
        },
    );

    test('emits font CSS variables alongside the palette and body/code reference them', () => {
        const css = composeStylesheet(null, {
            text: '"Source Sans Pro", sans-serif',
            mono: '"JetBrains Mono", monospace',
        });
        expect(css).toContain('--raven-font-text: "Source Sans Pro", sans-serif;');
        expect(css).toContain('--raven-font-mono: "JetBrains Mono", monospace;');
        expect(css).toContain('font-family: var(--raven-font-text)');
        expect(css).toContain('font-family: var(--raven-font-mono)');
    });

    test('font vars live outside the variant-conditional CSS so dark/light swap is colors-only', () => {
        // The `prefers-color-scheme: dark` media query rewrites the
        // palette but must NOT rewrite fonts — fonts don't vary by
        // variant. We verify by counting occurrences of the font
        // declarations: exactly one each in the entire stylesheet.
        const css = composeStylesheet(null, {
            text: 'Georgia, serif',
            mono: 'Menlo, monospace',
        });
        const textMatches = css.match(/--raven-font-text:/g) ?? [];
        const monoMatches = css.match(/--raven-font-mono:/g) ?? [];
        expect(textMatches.length).toBe(1);
        expect(monoMatches.length).toBe(1);
    });

    test('omitted fonts arg falls back to the historical hardcoded mono and a sensible body default', () => {
        // Bun tests that don't care about fonts still get a complete
        // stylesheet — back-compat for the unit-test surface.
        const css = composeStylesheet(null);
        expect(css).toContain('--raven-font-mono:');
        expect(css).toContain('--raven-font-text:');
        // The mono fallback is exactly the historical `baseStyles()`
        // string so existing snapshot-style assumptions about the
        // mono stack hold.
        expect(css).toContain('ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace');
    });
});

describe('sanitizeFontFamily', () => {
    test('accepts a simple comma list with quoted names', () => {
        expect(sanitizeFontFamily('"JetBrains Mono", "Fira Code", monospace'))
            .toBe('"JetBrains Mono", "Fira Code", monospace');
    });

    test('trims surrounding whitespace', () => {
        expect(sanitizeFontFamily('   Georgia, serif   ')).toBe('Georgia, serif');
    });

    test('rejects empty and whitespace-only input', () => {
        expect(sanitizeFontFamily('')).toBeNull();
        expect(sanitizeFontFamily('   ')).toBeNull();
        expect(sanitizeFontFamily('\t\n')).toBeNull();
    });

    test('rejects strings over 500 chars', () => {
        expect(sanitizeFontFamily('a'.repeat(501))).toBeNull();
        expect(sanitizeFontFamily('a'.repeat(500))).toBe('a'.repeat(500));
    });

    test.each([
        ';', '{', '}', '<', '>', '\\',
        '\n', '\r', '\t', '\f', '\v', '\0',
    ])(
        'rejects banned character %p',
        (banned) => {
            expect(sanitizeFontFamily(`Georgia${banned}serif`)).toBeNull();
        },
    );

    test('rejects CSS comment open/close sequences', () => {
        expect(sanitizeFontFamily('Georgia /* sneaky */ serif')).toBeNull();
        expect(sanitizeFontFamily('Georgia /*')).toBeNull();
        expect(sanitizeFontFamily('Georgia */ serif')).toBeNull();
    });

    test('rejects unmatched double quote', () => {
        // An unclosed `"` in the value would survive into the emitted
        // CSS as an open string token; recovery skips to the next `;`
        // and silently consumes the sibling --raven-font-mono
        // declaration. Reject up front.
        expect(sanitizeFontFamily('"Source Sans Pro')).toBeNull();
        expect(sanitizeFontFamily('Georgia, "Helvetica')).toBeNull();
    });

    test('rejects unmatched single quote', () => {
        // Same bad-string-token recovery hazard as the double-quote
        // case, just for the `'` form.
        expect(sanitizeFontFamily("Menlo, 'Andale Mono")).toBeNull();
        expect(sanitizeFontFamily("'Andale Mono")).toBeNull();
    });

    test('accepts mixed but balanced quotes', () => {
        expect(sanitizeFontFamily(`'JetBrains Mono', "Source Sans Pro"`))
            .toBe(`'JetBrains Mono', "Source Sans Pro"`);
    });

    test('accepts parens inside quoted family names', () => {
        // Real-world example: Microsoft Office's default body font is
        // registered with parens in the name. Quoted parens are part
        // of the string content per CSS, so they're safe.
        expect(sanitizeFontFamily('"Aptos (Body)", sans-serif'))
            .toBe('"Aptos (Body)", sans-serif');
        expect(sanitizeFontFamily("'Aptos (Body)', sans-serif"))
            .toBe("'Aptos (Body)', sans-serif");
        expect(sanitizeFontFamily('"Aptos (Body)"'))
            .toBe('"Aptos (Body)"');
    });

    test('rejects bare parens outside a quoted family name', () => {
        // Bare `(` opens a CSS function-token whose consumption ignores
        // `}` boundaries — would corrupt the rest of the stylesheet.
        // Bare balanced `(Foo)` is also rejected: CSS's unquoted
        // family-name grammar doesn't accept parens, so the declaration
        // would IACVT-drop anyway; rejecting up front keeps the rule
        // simple and consistent.
        expect(sanitizeFontFamily('Foo(bar')).toBeNull();
        expect(sanitizeFontFamily('Foo (Bar)')).toBeNull();
        expect(sanitizeFontFamily('Foo(Bar)')).toBeNull();
        expect(sanitizeFontFamily('"Foo" (Bar)')).toBeNull();
        expect(sanitizeFontFamily('("Foo")')).toBeNull();
    });

    test('rejects trailing comma', () => {
        // `Foo,` becomes `Foo,, sans-serif` after the resolver appends
        // a generic, which makes the var() substitution invalid and
        // the font-family declaration is dropped at IACVT.
        expect(sanitizeFontFamily('Georgia,')).toBeNull();
        expect(sanitizeFontFamily('Georgia, ')).toBeNull();
    });

    test('rejects leading and consecutive commas', () => {
        expect(sanitizeFontFamily(',Arial')).toBeNull();
        expect(sanitizeFontFamily('Arial,,Times')).toBeNull();
        expect(sanitizeFontFamily('Arial, , Times')).toBeNull();
    });

    test('rejects empty quoted family entries', () => {
        // `""` and `''` are degenerate — CSS treats them as a
        // custom family with no name, which no font matches and the
        // user almost certainly did not mean.
        expect(sanitizeFontFamily('""')).toBeNull();
        expect(sanitizeFontFamily("''")).toBeNull();
        expect(sanitizeFontFamily('"", monospace')).toBeNull();
        expect(sanitizeFontFamily('Arial, ""')).toBeNull();
    });

    test.each([
        'inherit', 'initial', 'unset', 'revert', 'revert-layer',
        'INHERIT', 'Initial', 'Unset', 'Revert', 'Revert-Layer',
    ])(
        'rejects bare CSS-wide keyword %p',
        (keyword) => {
            // CSS-wide keywords are only valid as the sole value of a
            // property; the iframe has no useful parent for `inherit`
            // to pull from. Reject so the user falls through to a
            // real font instead of seeing UA-default Times.
            expect(sanitizeFontFamily(keyword)).toBeNull();
        },
    );

    test('accepts CSS-wide keyword in a list (treated as bare identifier)', () => {
        // `Foo, inherit` is valid CSS — `inherit` becomes a literal
        // (unknown) family name and the browser falls through. We do
        // NOT reject this because the list shape is well-formed and
        // the user's other entries still work.
        expect(sanitizeFontFamily('Foo, inherit')).toBe('Foo, inherit');
    });

    test('non-string input is rejected', () => {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        expect(sanitizeFontFamily(null as any)).toBeNull();
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        expect(sanitizeFontFamily(undefined as any)).toBeNull();
    });
});

describe('resolveFontFamilies', () => {
    test('prefers the raven-knit setting over the VS Code fallback', () => {
        const out = resolveFontFamilies(
            'Georgia, serif',
            '"JetBrains Mono", monospace',
            'system-ui, sans-serif',
            'Menlo, Consolas, monospace',
        );
        expect(out.text).toBe('Georgia, serif');
        expect(out.mono).toBe('"JetBrains Mono", monospace');
    });

    test('falls through to the VS Code fallback when the raven setting is empty', () => {
        const out = resolveFontFamilies(
            '',
            '',
            'system-ui, sans-serif',
            'Menlo, Consolas, monospace',
        );
        expect(out.text).toBe('system-ui, sans-serif');
        expect(out.mono).toBe('Menlo, Consolas, monospace');
    });

    test('falls through to the hardcoded fallback when both upstreams are rejected', () => {
        const out = resolveFontFamilies('Geo;rgia', 'Menlo{}', '', '');
        // Banned chars in the primary AND empty fallbacks → hardcoded.
        expect(out.text).toBe(
            '-apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif',
        );
        expect(out.mono).toBe(
            'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
        );
    });

    test('falls through past a hostile VS Code fallback to the hardcoded default', () => {
        const out = resolveFontFamilies('', '', 'evil;font', 'evil{font}');
        expect(out.text).toContain('system-ui'); // hardcoded text fallback
        expect(out.mono).toContain('ui-monospace'); // hardcoded mono fallback
    });

    test('appends a generic-family terminator when missing', () => {
        const out = resolveFontFamilies(
            '"Source Sans Pro"',
            '"JetBrains Mono"',
            '',
            '',
        );
        expect(out.text).toBe('"Source Sans Pro", sans-serif');
        expect(out.mono).toBe('"JetBrains Mono", monospace');
    });

    test('does NOT duplicate a terminator that is already present', () => {
        const out = resolveFontFamilies(
            'Georgia, serif',
            '"JetBrains Mono", monospace',
            '',
            '',
        );
        expect(out.text).toBe('Georgia, serif');
        expect(out.mono).toBe('"JetBrains Mono", monospace');
    });

    test('recognizes all CSS generic-family keywords as terminators', () => {
        // Mirror the GENERIC_FAMILY_KEYWORDS set in render-html.ts —
        // adding/removing an entry there should be reflected here so
        // a stale set silently introduces double-terminators.
        for (const generic of [
            'monospace', 'sans-serif', 'serif', 'system-ui',
            'ui-monospace', 'ui-sans-serif', 'ui-serif', 'ui-rounded',
            'cursive', 'fantasy', 'emoji', 'math', 'fangsong',
        ]) {
            const out = resolveFontFamilies(`Foo, ${generic}`, `Bar, ${generic}`, '', '');
            expect(out.text).toBe(`Foo, ${generic}`);
            expect(out.mono).toBe(`Bar, ${generic}`);
        }
    });

    test('terminator check is case-insensitive', () => {
        const out = resolveFontFamilies('Georgia, SERIF', 'Menlo, MonoSpace', '', '');
        expect(out.text).toBe('Georgia, SERIF');
        expect(out.mono).toBe('Menlo, MonoSpace');
    });

    test('treats a quoted "monospace" as a family name, not a generic terminator', () => {
        // CSS treats `"monospace"` (quoted) as a custom family name,
        // NOT the generic keyword. The terminator should still be
        // appended so the browser has a real generic fallback.
        const out = resolveFontFamilies('', '"monospace"', '', '');
        expect(out.mono).toBe('"monospace", monospace');
    });

    test('handles commas inside quoted family names without breaking terminator detection', () => {
        // Top-level comma split must respect quotes — a single quoted
        // family with a comma inside its name should be treated as
        // one entry, not two.
        const out = resolveFontFamilies('', '"Comma, Foundry"', '', '');
        expect(out.mono).toBe('"Comma, Foundry", monospace');
    });
});

describe('renderKnitHtml', () => {
    /** Toy R tokenizer (same shape as the code-highlighter test). */
    function toyRTokenizer(line: string): ScopeToken[] {
        const tokens: ScopeToken[] = [];
        let i = 0;
        while (i < line.length) {
            const ch = line[i];
            if (/[A-Za-z]/.test(ch)) {
                let j = i + 1;
                while (j < line.length && /[A-Za-z0-9_.]/.test(line[j])) j++;
                tokens.push({ startIndex: i, endIndex: j, scopes: ['source.r', 'entity.name.function.r'] });
                i = j;
                continue;
            }
            tokens.push({ startIndex: i, endIndex: i + 1, scopes: ['source.r'] });
            i++;
        }
        return tokens;
    }

    test('rewrites an R code block with grammar-aware spans', async () => {
        // markdown.api.render produces this shape (highlight.js spans
        // around the source). We use a simpler raw form here.
        const fakeHtml =
            '<p>prose</p>\n<pre><code class="language-r">library</code></pre>\n<p>after</p>';
        const out = await renderKnitHtml({
            markdownSource: 'irrelevant — renderMarkdown returns a fixed value',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
        });

        // The body must include a function-role span. Spans
        // reference the palette via the `--raven-c-${role}` CSS
        // variable so the surrounding stylesheet's palette swap
        // (e.g. browser dark-mode `@media` swap) reaches them.
        expect(out).toContain(
            `<span style="color:var(--raven-c-function)">library</span>`,
        );
        // Surrounding prose is preserved.
        expect(out).toContain('<p>prose</p>');
        expect(out).toContain('<p>after</p>');
    });

    test('leaves untagged code blocks intact', async () => {
        const fakeHtml = '<pre><code>raw text &amp; symbols</code></pre>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
        });

        // The bare `<pre><code>raw text &amp; symbols</code></pre>`
        // must round-trip without spans or escaping changes — and
        // crucially without the `raven-knit-code` marker class, so
        // the base stylesheet leaves it bare (no border, no panel
        // background). knitr emits R output as untagged fenced blocks,
        // so this is the path that lets readers tell input from
        // output the way Quarto's preview does. The literal here
        // covers both: the exact `<pre><code>` shape (no class) AND
        // the unchanged inner text.
        expect(out).toContain('<pre><code>raw text &amp; symbols</code></pre>');
    });

    test('tags highlighted code blocks with the raven-knit-code marker', async () => {
        // Input (highlighted) `<pre>` blocks carry the
        // `raven-knit-code` class so the base stylesheet's chrome
        // (border, padding, background) targets ONLY input chunks.
        // Output blocks — emitted by knitr without a `language-X`
        // tag — never gain the marker and render as bare monospace.
        const fakeHtml = '<pre><code class="language-r">library</code></pre>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
        });
        expect(out).toContain('<pre class="raven-knit-code"><code class="language-r">');
    });

    test('base stylesheet scopes panel chrome to pre.raven-knit-code', async () => {
        // The chrome (border, padding, border-radius, panel
        // background) must target `pre.raven-knit-code` only.
        // Untagged `<pre>` (output) gets nothing more than a
        // monospace font from the inner `<code>` rule, so the panel
        // visual is unique to input chunks.
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => '<p>hi</p>',
            registry: fakeRegistry({}),
        });
        // The chrome block keys off the marker class.
        expect(out).toMatch(/pre\.raven-knit-code\s*\{[^}]*border:/);
        expect(out).toMatch(/pre\.raven-knit-code\s*\{[^}]*padding:/);
        // No `pre { padding: ... }` rule for the bare selector — that
        // would re-paint output blocks with chrome.
        expect(out).not.toMatch(/(^|\W)pre\s*\{\s*padding:/);
    });

    test('overlays R function semantic tokens on top of the grammar', async () => {
        // Pretend the grammar mis-tokenizes the identifier as a
        // variable; the LSP overlay should still promote it to
        // function color.
        const fakeHtml = '<pre><code class="language-r">library</code></pre>';
        const flatTokenizer = (_line: string): ScopeToken[] => [{
            startIndex: 0,
            endIndex: 7,
            scopes: ['source.r', 'variable.other.r'],
        }];
        // LSP semantic tokens for `library` (7 chars from col 0, line 0).
        const tokensData = [0, 0, 7, 0, 0];
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: flatTokenizer }),
            fetchRSemanticTokens: async () => tokensData,
        });

        expect(out).toContain(
            `<span style="color:var(--raven-c-function)">library</span>`,
        );
    });

    test('non-R blocks do not call fetchRSemanticTokens', async () => {
        const fakeHtml = '<pre><code class="language-python">import math</code></pre>';
        let fetchCalled = false;
        await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ python: toyRTokenizer }),
            fetchRSemanticTokens: async () => {
                fetchCalled = true;
                return [];
            },
        });
        expect(fetchCalled).toBe(false);
    });

    test('inlines KaTeX CSS when provided', async () => {
        const fakeHtml = '<p>math</p>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({}),
            katexCss: '.katex { color: blue; }',
        });
        expect(out).toContain('.katex { color: blue; }');
    });

    test('produces a self-contained document with doctype, head, body', async () => {
        const fakeHtml = '<p>hi</p>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({}),
        });
        expect(out).toMatch(/^<!doctype html>/i);
        expect(out).toContain('<head>');
        expect(out).toContain('<body>');
        expect(out).toContain('<p>hi</p>');
    });

    test('decodes HTML entities in the code block before tokenizing', async () => {
        // `&lt;-` is markdown-it's escape of `<-`. The decoder must
        // turn it back into `<-` before the toy tokenizer sees it, so
        // the toy tokenizer's "letters → function" rule fires on
        // `f` rather than on `f&` or some malformed sequence.
        const fakeHtml = '<pre><code class="language-r">f &lt;- 1</code></pre>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
        });
        // The `<` MUST be HTML-escaped in the OUTPUT (because we're
        // re-encoding code-block text for HTML safety). The
        // `<span>` wrapper around `f` MUST appear at start.
        expect(out).toContain(
            `<span style="color:var(--raven-c-function)">f</span>`,
        );
        expect(out).toContain('&lt;-');
    });

    test('emits a dark palette when themeClasses says dark', async () => {
        const fakeHtml = '<pre><code class="language-r">library</code></pre>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
            themeClasses: 'vscode-dark',
        });
        // The span itself references the CSS variable (uniform shape
        // across all themes). The "this is dark" property lives in
        // the stylesheet: under `vscode-dark`, `composeStylesheet`
        // only emits the dark palette into `:root`, so the var()
        // references resolve to the dark function color.
        expect(out).toContain(
            `<span style="color:var(--raven-c-function)">library</span>`,
        );
        expect(out).toContain(`--raven-c-function: ${githubDark.roles.function};`);
        expect(out).not.toContain(`--raven-c-function: ${githubLight.roles.function};`);
        expect(out).toContain(githubDark.background);
        expect(out).not.toContain(githubLight.background);
    });

    test('standalone output paints spans via CSS variables so browser dark-mode reaches them', async () => {
        // Regression: opening the rendered `<basename>.html` in a
        // browser that resolves `prefers-color-scheme: dark` was
        // flipping the page background to dark but leaving the
        // syntax-highlighted code spans on the LIGHT palette colors.
        //
        // The `composeStylesheet(null)` path emits both palettes and
        // swaps `--raven-c-*` variables inside an `@media (prefers-
        // color-scheme: dark)` block, but spans were emitting
        // `style="color:#8250df"` (the light function hex) baked at
        // render time. An inline `style="color:..."` always wins over
        // the variable-resolved palette rule, so the swap had no
        // effect on the code spans.
        //
        // Spans MUST therefore reference the CSS variable rather than
        // baked hex so the dark-palette swap actually reaches them.
        const fakeHtml = '<pre><code class="language-r">library</code></pre>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
            themeClasses: null, // standalone (Open in Browser path)
        });

        // Function-role span MUST resolve its color through the
        // CSS variable, not a static palette hex.
        expect(out).toContain(
            '<span style="color:var(--raven-c-function)">library</span>',
        );
        // Sanity: it must NOT bake the light-palette function hex
        // into the span — that's the symptom of the bug.
        expect(out).not.toContain(
            `<span style="color:${githubLight.roles.function}">library</span>`,
        );
        // Sanity: the dark-palette `@media` swap is the thing the
        // CSS variable resolves through under dark mode.
        expect(out).toMatch(/@media\s*\(\s*prefers-color-scheme:\s*dark\s*\)/);
        expect(out).toContain(`--raven-c-function: ${githubDark.roles.function};`);
    });

    test('falls back to grammar-only when fetchRSemanticTokens throws', async () => {
        const fakeHtml = '<pre><code class="language-r">library</code></pre>';
        const out = await renderKnitHtml({
            markdownSource: 'x',
            renderMarkdown: async () => fakeHtml,
            registry: fakeRegistry({ r: toyRTokenizer }),
            fetchRSemanticTokens: async () => {
                throw new Error('lsp unreachable');
            },
        });
        // The toy tokenizer scopes letters as functions, so we still
        // get a function span — the overlay just isn't applied.
        expect(out).toContain(
            `<span style="color:var(--raven-c-function)">library</span>`,
        );
    });

    test('strips YAML frontmatter from the markdown before calling renderMarkdown', async () => {
        // Regression test for the user-reported "Knit Preview prints
        // YAML as a table" behavior. The strip lives in renderKnitHtml
        // so VS Code's markdown.api.render never sees the frontmatter
        // and therefore never emits its `<table class="frontmatter">`
        // shape. The on-disk .md is untouched (Pandoc export depends on
        // the full YAML); this test only exercises the in-memory
        // strip handed to the injected renderMarkdown.
        const seenByRenderer: string[] = [];
        const markdownSource =
            '---\ntitle: Hi\nauthor: Me\n---\n\nBody text.\n';
        const out = await renderKnitHtml({
            markdownSource,
            renderMarkdown: async (src) => {
                seenByRenderer.push(src);
                return '<p>Body text.</p>';
            },
            registry: fakeRegistry({}),
        });

        // renderMarkdown received the post-strip source — no YAML
        // delimiters, no `title:` line.
        expect(seenByRenderer).toHaveLength(1);
        expect(seenByRenderer[0]).not.toContain('---');
        expect(seenByRenderer[0]).not.toContain('title:');
        expect(seenByRenderer[0]).not.toContain('author:');
        expect(seenByRenderer[0]).toContain('Body text.');

        // The assembled document body must not contain YAML fragments
        // either. We deliberately do not assert on the `<table
        // class="frontmatter">` shape here — that's VS Code's
        // markdown extension's emission, which we're not exercising
        // with the fake renderer. The end-to-end integration test in
        // editors/vscode/src/test/knit-preview-yaml-stripped.test.ts
        // covers that shape against the real renderer.
        expect(out).toContain('<p>Body text.</p>');
        expect(out).not.toContain('title:');
        expect(out).not.toContain('author:');
    });
});
