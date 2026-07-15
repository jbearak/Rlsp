/**
 * Inline relative `<img src>` references in a rendered HTML document
 * as `data:` URLs read from disk. The function is the workaround for
 * a nested-iframe subresource issue in the Knit Preview panel.
 *
 * Why this exists
 * ---------------
 *
 * The Knit Preview webview shell wraps the rendered HTML in a nested
 * `<iframe srcdoc>` (see `knit-output.ts`). VS Code's webview
 * resource handler intercepts requests issued from the OUTER webview
 * document, but it does NOT intercept subresource fetches (`<img>`,
 * `<link>`, `<video>`, etc.) issued from a NESTED iframe — the
 * Electron protocol handler only sees top-level webview navigations.
 *
 * The visible failure mode: a `<base href="webview-resource://…">`
 * resolves an `<img src="figure/plot-1.png">` to a URL like
 * `https://file+.vscode-resource.vscode-cdn.net/.../figure/plot-1.png`,
 * which escapes the protocol handler and hits the real network
 * stack. The DNS lookup for `file+.vscode-resource.vscode-cdn.net`
 * fails, the image element fires `load` with `naturalWidth === 0`,
 * and the user sees the broken-image icon — even though the same
 * `.html` opened directly in a browser renders the image fine.
 *
 * The fix: pre-process the HTML at panel-render time, read each
 * relative image file from disk, encode as a `data:` URL, and
 * substitute it back into the `src` attribute. `data:` URLs are
 * scheme-internal and never touch the protocol handler, so they
 * survive the nested-iframe boundary unchanged.
 *
 * Only the in-memory copy handed to the iframe is rewritten. The
 * on-disk `.html` the post-knit renderer wrote keeps file-relative
 * `<img>` paths, so "Open in Browser" still produces a small file
 * with the original asset references.
 *
 * Local images outside `docDir`
 * -----------------------------
 *
 * `knitr::include_graphics()` can reference an image that already
 * exists elsewhere in the workspace (issue #627), e.g. an absolute
 * `<img src="/project/images/logo.png">` or a relative one that walks
 * out of the temp preview dir (`../images/logo.png`). Those must
 * inline too — the webview has no filesystem access, and the same
 * rendered HTML feeds "Open in Browser", so `asWebviewUri()` isn't an
 * option. We therefore allow a file when it is contained by ANY of
 * the caller-supplied `allowedRoots` (the temp preview dir plus,
 * typically, the workspace folder containing the source document).
 *
 * Security notes
 * --------------
 *
 *   - Absolute URLs (http/https/data/file/etc.) are passed through
 *     untouched; this function only inlines filesystem paths.
 *   - The resolved file and every allowed root are canonicalized with
 *     `realpath` before the containment check, so a symlink inside a
 *     root cannot point the inliner at a file outside it. A file that
 *     resolves outside every allowed root (e.g. `../../etc/passwd`) is
 *     left in place so the user gets a visible failure instead of a
 *     silent file-read.
 *   - Relative paths resolve against `docDir` (the rendered document's
 *     directory) first, then any `resolveBases` (the knit `root.dir`,
 *     for author `include_graphics` paths); absolute paths resolve
 *     as-is. Either way the result must land inside an allowed root.
 *   - Unknown extensions (anything not in
 *     `mimeForImageExtension`) are passed through; we don't read
 *     arbitrary file types off disk in case a future markdown
 *     pipeline starts producing `<img>` to non-image resources.
 *
 * Tests live in `tests/bun/inline-local-images.test.ts`.
 */
import * as fs from 'fs';
import * as path from 'path';
import { escapeHtml } from './code-highlighter';

/**
 * The minimum surface a logging sink needs to receive an inlining
 * failure message. Production code passes a `vscode.OutputChannel`;
 * tests can pass an in-memory collector or omit.
 */
export interface InlineImagesOutputSink {
    appendLine(line: string): void;
}

export interface InlineImagesOptions {
    /**
     * Mark local `figure/*.svg` images so the Knit Preview shell can
     * replace them with sanitized inline SVG nodes inside the sandboxed
     * iframe. The marker is opt-in because this helper is also useful
     * as a plain "data URL all local images" transform in tests and
     * future callers.
     */
    markSvgPlots?: boolean;
    /**
     * Directories — beyond `docDir`, which is always allowed — whose
     * images may be inlined (issue #627). The Knit Preview panel passes
     * the workspace folder containing the source document (or, for a
     * loose file with no workspace, the source document's own
     * directory) so a `knitr::include_graphics()` reference to an
     * existing workspace image resolves. Each root is canonicalized
     * with `realpath`; a root that doesn't exist is skipped.
     */
    additionalRoots?: string[];
    /**
     * Directories — beyond `docDir`, which is always tried first — to
     * resolve a RELATIVE `<img src>` against (issue #627). A rendered
     * document mixes two kinds of relative path with different correct
     * bases: knitr's generated plots (`figure/plot-1.png`) are relative
     * to the preview output dir (`docDir`), while an author's
     * `knitr::include_graphics("images/logo.png")` is relative to the
     * knit working directory (`root.dir`). The panel passes `root.dir`
     * here so both resolve; bases are tried in order, `docDir` first.
     * Every base is automatically also an allowed containment root (a
     * base you can resolve against but not read from would be useless),
     * so callers need not repeat it in `additionalRoots`.
     */
    resolveBases?: string[];
}

export function inlineLocalImagesAsDataUrls(
    html: string,
    docDir: string,
    output?: InlineImagesOutputSink,
    options: InlineImagesOptions = {},
): string {
    // Canonicalize the allowed roots ONCE (not per image): a document
    // with N images and R roots would otherwise do O(N×R) synchronous
    // `realpath` calls on the extension host. Non-existent roots drop
    // out here. Every `resolveBases` entry is also an allowed root — a
    // base you resolve against but can't read from would be useless, and
    // requiring callers to repeat it in `additionalRoots` is a trap.
    const canonicalRoots = canonicalizeRoots([
        docDir,
        ...(options.additionalRoots ?? []),
        ...(options.resolveBases ?? []),
    ]);
    // Quote-aware `<img>` matcher: the attribute run is a sequence of
    // double-quoted strings, single-quoted strings, or non-quote /
    // non-`>` characters, so a literal `>` inside an attribute value
    // (e.g. `alt="a > b"`) doesn't prematurely terminate the tag. The
    // three alternatives are mutually exclusive (`[^>"']` excludes both
    // quotes), so there is no ambiguity to backtrack over.
    return html.replace(/<img\b((?:"[^"]*"|'[^']*'|[^>"'])*)>/gi, (match, attrs: string) => {
        // Locate the `src` attribute with a quote-aware tokenizer rather
        // than a regex over the raw attribute blob. A regex can't tell
        // an `src=` at an attribute boundary from `src=`-looking text
        // *inside* another attribute's quoted value (e.g.
        // `alt="see src='x'"`) or from a different attribute name that
        // ends in `src` (`data-src`, which a `\bsrc` would match on the
        // `-` boundary). The tokenizer consumes each quoted value whole,
        // so only a real `src` attribute is selected.
        const srcAttr = findAttribute(attrs, 'src');
        if (!srcAttr) return match;

        // Decode HTML entities up front. An HTML parser resolves entity
        // references in an attribute value before anything consumes it,
        // so every guard and split below sees what the browser sees:
        //   - `https&#58;//example.com/x.png` is the remote URL
        //     `https://example.com/x.png` (so the scheme guard must run
        //     on the decoded value, not the raw one);
        //   - a workspace image `a&b.png` emitted as `a&amp;b.png` or the
        //     numeric `a&#38;b.png` decodes to `a&b.png`;
        //   - decoding before the URL split also stops a numeric
        //     reference like `&#38;` from having its `#` misread as a
        //     fragment delimiter.
        const src = decodeHtmlEntities(srcAttr.value);

        // Already an absolute URL (any scheme, e.g. `https:`,
        // `data:`, `vscode-webview:`, `file:`) — pass through. A
        // Windows drive path (`C:\…`) also matches the single-letter
        // "scheme" shape, so on Windows exclude it: it's an absolute
        // filesystem path we may still want to inline. The platform
        // gate matters — off Windows a `C:\…` string is not a real
        // path, and treating a genuine one-letter URL scheme (`x:/…`)
        // as a drive would be wrong, so there we let the scheme guard
        // pass it through untouched.
        const isWindowsDrivePath = process.platform === 'win32'
            && /^[a-z]:[\\/]/i.test(src);
        if (!isWindowsDrivePath && /^(?:[a-z][a-z0-9+\-.]*:)/i.test(src)) return match;
        // Protocol-relative URL.
        if (src.startsWith('//')) return match;

        // Split into path, `?query`, and `#fragment`. htmlwidgets and
        // similar renderers emit cache-busters (`plot.png?v=1`) and SVG
        // view fragments (`diagram.svg#layer-1`). Feeding the whole value
        // to `path.resolve` / `path.extname` would fold the suffix into
        // the filename (wrong file, and `.png?v=1` maps to no MIME).
        //
        // Only the fragment rides along on the emitted data URL: a
        // fragment is a real URL component (split off before the data is
        // decoded) and selects a named SVG `<view>`. The query is
        // DROPPED — a data URL has no cache to bust, and per WHATWG
        // forgiving-base64 a `?` in the data portion is an invalid
        // base64 code point that fails the decode, so appending it would
        // break the image.
        const hashIdx = src.indexOf('#');
        const fragment = hashIdx >= 0 ? src.slice(hashIdx) : '';
        const beforeHash = hashIdx >= 0 ? src.slice(0, hashIdx) : src;
        const queryIdx = beforeHash.indexOf('?');
        const srcPath = queryIdx >= 0 ? beforeHash.slice(0, queryIdx) : beforeHash;
        const srcSuffix = fragment;

        // A browser percent-DECODES a `src` path before hitting the
        // filesystem (VS Code's `markdown.api.render` percent-encodes
        // paths, so a workspace image `café.png` arrives as
        // `caf%C3%A9.png`). Try the decoded spelling FIRST — matching
        // what the browser and Open-in-Browser would load — and fall
        // back to the literal spelling only if the decoded one doesn't
        // resolve, which also covers a filename that genuinely contains
        // a `%`.
        const pathCandidates = percentDecodedCandidates(srcPath);

        // Relative-path resolution bases, tried in order. `docDir` (the
        // preview output dir) comes first — knitr's generated plots
        // (`figure/plot-1.png`) live there — then any caller-supplied
        // bases (the knit `root.dir`, against which an author's
        // `include_graphics("images/logo.png")` resolves). An absolute
        // path ignores the bases and resolves as-is. The leading `./`
        // strip doesn't change semantics — it just keeps
        // `normalizedRelative` (used for the figure-plot marker) clean.
        const relativeBases = [docDir, ...(options.resolveBases ?? [])];

        let realResolved: string | null = null;
        // The logical (pre-realpath) resolved path that MATCHED — used
        // only for the figure-plot marker (relative to docDir). The
        // data-URL MIME is derived separately from the CANONICAL file.
        let logicalResolved: string | null = null;
        let mime: string | null = null;
        // Every path we resolved and tried to canonicalize, so a failure
        // diagnostic can name all the bases attempted (docDir AND
        // root.dir) rather than only the first. A non-empty `attempted`
        // also doubles as "at least one candidate had a known image
        // extension" (both happen on the same branch past the `candMime`
        // guard below), so no separate flag is needed.
        const attempted: string[] = [];
        // BASE is the outer loop so `docDir` is exhausted (both path
        // spellings) before `root.dir` — the documented docDir-first
        // precedence. SPELLING is inner so within a base the decoded
        // form wins over the literal percent-encoded twin. Absolute
        // paths ignore the bases (resolve as-is). Abs-ness is taken from
        // the literal `srcPath`; even if percent-decoding a candidate
        // produced a leading `/`, `path.resolve(base, "/abs")` still
        // yields the absolute path, so the classification is harmless.
        const isAbsolute = path.isAbsolute(srcPath);
        const bases = isAbsolute ? [undefined] : relativeBases;
        outer:
        for (const base of bases) {
            for (const cand of pathCandidates) {
                const candMime = mimeForImageExtension(path.extname(cand).toLowerCase());
                // Unknown extensions are passed through; we don't read
                // arbitrary file types off disk.
                if (!candMime) continue;
                const cleaned = isAbsolute ? cand : cand.replace(/^\.\//, '');
                const resolved = base === undefined
                    ? path.resolve(cleaned)
                    : path.resolve(base, cleaned);
                attempted.push(resolved);
                // Canonicalize the candidate against the allowed roots
                // with `realpath` (so a symlink can't escape a root) and
                // require containment. Fails closed: a missing file, a
                // symlink target outside every root, or a path that
                // simply lives elsewhere all leave the `src` untouched.
                const real = canonicalizeContainedPath(resolved, canonicalRoots);
                if (real !== null) {
                    realResolved = real;
                    logicalResolved = resolved;
                    // MIME must match the bytes we actually read, which
                    // come from the canonical (realpath) file — a symlink
                    // `logo.png` → `logo.svg` must be labeled
                    // `image/svg+xml`, not `image/png`. Fall back to the
                    // requested extension's type when the canonical file
                    // has no / an unknown extension.
                    mime = mimeForImageExtension(path.extname(real).toLowerCase()) ?? candMime;
                    break outer;
                }
            }
        }
        // No candidate had a known image extension — leave the tag alone.
        if (attempted.length === 0) return match;
        if (realResolved === null || logicalResolved === null || mime === null) {
            // The path looked inlinable but didn't resolve inside an
            // allowed root (missing, unreadable, or outside the
            // workspace). Surface every base tried so a broken image in
            // the preview has a matching line in the Raven Knit output
            // channel that names the paths the author can actually fix.
            output?.appendLine(
                `[panel] not inlining image ${JSON.stringify(src)} `
                + '(missing, unreadable, or outside the preview/workspace roots); tried: '
                + attempted.join(', '),
            );
            return match;
        }

        // The figure-plot marker keys off the path relative to `docDir`
        // (its logical, pre-realpath form), so it only ever matches the
        // preview-generated `docDir/figure/*` tree, not workspace images.
        const normalizedRelative = path.relative(path.resolve(docDir), logicalResolved);

        let bytes: Buffer;
        try {
            bytes = fs.readFileSync(realResolved);
        } catch (err) {
            output?.appendLine(
                `[panel] could not inline image ${realResolved}: ${
                    err instanceof Error ? err.message : String(err)
                }`,
            );
            return match;
        }

        // `srcSuffix` (the `#fragment`) was HTML-entity-DECODED with the
        // rest of `src`, so it can now contain a raw `"` or `&` (e.g.
        // from `&quot;`/`&amp;` in the source). Re-escape for the
        // double-quoted attribute context before splicing it back, or a
        // decoded `"` would break out of the `src="…"` attribute and
        // corrupt the tag. (The `data:…;base64,` prefix and the base64
        // payload contain no attribute-special characters, so escaping
        // is a no-op for them.)
        const dataUrl = `data:${mime};base64,${bytes.toString('base64')}${srcSuffix}`;
        const rewrittenAttrs = attrs.slice(0, srcAttr.start)
            + `src="${escapeHtml(dataUrl)}"`
            + attrs.slice(srcAttr.end);
        // Key the SVG-plot marker off the resolved MIME (canonical file),
        // consistent with the data URL — so a figure that is actually SVG
        // is themed even if reached through a differently-suffixed name.
        const finalAttrs = options.markSvgPlots && mime === 'image/svg+xml'
            && isKnitFigurePath(normalizedRelative)
            ? withImgAttribute(rewrittenAttrs, 'data-raven-plot-svg', 'true')
            : rewrittenAttrs;
        return `<img${finalAttrs}>`;
    });
}

interface HtmlAttribute {
    /** The attribute name as written (original case). */
    name: string;
    /** The attribute value with surrounding quotes stripped. */
    value: string;
    /**
     * Half-open `[start, end)` offsets of the matched `name=value` span
     * within the attribute string. Splice by these offsets rather than
     * `String.prototype.replace(matchText, …)`: an identical
     * `src=value` substring can also appear inside an EARLIER
     * attribute's quoted value (`alt="src='x'"`), and a textual replace
     * would hit that first occurrence instead of the real attribute.
     */
    start: number;
    end: number;
}

/**
 * Find the first attribute named `name` (case-insensitive) in an HTML
 * tag's attribute string. The scan is quote-aware — each attribute's
 * quoted value is consumed whole — so `name=`-looking text inside
 * another attribute's value (e.g. `alt="see src='x'"`) is never
 * mistaken for a real attribute, and a differently-named attribute that
 * merely ends in `name` (e.g. `data-src`) is not matched. Returns the
 * attribute, or `null` if absent.
 */
function findAttribute(attrs: string, name: string): HtmlAttribute | null {
    // name  — a run with no whitespace, `=`, `/`, `>`, or quote.
    // value — double-quoted | single-quoted | unquoted run (optional).
    const re = /([^\s=/>"']+)(?:\s*=\s*("[^"]*"|'[^']*'|[^\s>]*))?/g;
    const target = name.toLowerCase();
    let m: RegExpExecArray | null;
    while ((m = re.exec(attrs)) !== null) {
        if (m[1].toLowerCase() !== target) continue;
        const raw = m[2];
        let value = '';
        if (raw !== undefined) {
            const quoted = raw.length >= 2
                && ((raw[0] === '"' && raw.endsWith('"'))
                    || (raw[0] === "'" && raw.endsWith("'")));
            value = quoted ? raw.slice(1, -1) : raw;
        }
        return { name: m[1], value, start: m.index, end: m.index + m[0].length };
    }
    return null;
}

/**
 * The distinct path spellings to try (already entity-decoded),
 * percent-DECODED first. A browser percent-decodes a `src` path before
 * hitting the filesystem, so the decoded spelling is what actually
 * loads — trying it first means that when both `a b.png` and a literal
 * `a%20b.png` exist, `src="a%20b.png"` inlines `a b.png`, matching the
 * browser. The literal spelling is kept as a fallback so a filename that
 * genuinely contains a `%` (whose decoded form doesn't exist) still
 * resolves.
 */
function percentDecodedCandidates(srcPath: string): string[] {
    let decoded = srcPath;
    try {
        decoded = decodeURIComponent(srcPath);
    } catch {
        // Malformed percent-escape (e.g. a literal `%` in the name) —
        // keep the literal form; the fallback below covers it too.
    }
    return decoded === srcPath ? [srcPath] : [decoded, srcPath];
}

/** The named HTML entities a markdown/HTML renderer emits in a path. */
const NAMED_ENTITIES: Readonly<Record<string, string>> = {
    amp: '&',
    lt: '<',
    gt: '>',
    quot: '"',
    apos: "'",
};

/**
 * Decode the HTML entities a markdown/HTML renderer emits inside an
 * attribute value — the common named ones plus decimal (`&#38;`) and
 * hex (`&#x26;`) numeric references. A single left-to-right pass decodes
 * each entity exactly once and never rescans its own output, so a
 * double-encoded sequence like `&amp;lt;` correctly collapses to `&lt;`
 * rather than `<`. An out-of-range or malformed numeric reference is
 * left verbatim.
 */
function decodeHtmlEntities(s: string): string {
    return s.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (whole, body: string) => {
        if (body[0] === '#') {
            const cp = (body[1] === 'x' || body[1] === 'X')
                ? parseInt(body.slice(2), 16)
                : parseInt(body.slice(1), 10);
            if (!Number.isFinite(cp) || cp < 0 || cp > 0x10ffff) return whole;
            try {
                return String.fromCodePoint(cp);
            } catch {
                return whole;
            }
        }
        const decoded = NAMED_ENTITIES[body.toLowerCase()];
        return decoded ?? whole;
    });
}

/**
 * `realpath`-resolve each root once, dropping any that don't exist and
 * de-duplicating the results (callers routinely pass the same directory
 * twice — e.g. in `project` mode the workspace folder and the knit
 * `root.dir` are identical). Hoisted out of the per-image loop so
 * containment is O(N + R), not O(N×R), synchronous `realpath` calls.
 */
function canonicalizeRoots(roots: string[]): string[] {
    const seen = new Set<string>();
    // De-duplicate the INPUT strings first (callers routinely pass the
    // same directory twice — e.g. `project` mode's workspace folder and
    // knit `root.dir` are identical) so we don't `realpath` it twice.
    for (const root of new Set(roots)) {
        try {
            seen.add(fs.realpathSync(root));
        } catch {
            // A root that doesn't exist can contain nothing — skip it.
        }
    }
    return [...seen];
}

/**
 * Canonicalize `candidate` with `realpath` and return the real path
 * only if it is contained by (or equal to) at least one already-
 * canonicalized `canonicalRoots` entry; otherwise `null`.
 *
 * Both sides are `realpath`-resolved so a symlink living inside a root
 * cannot smuggle in a target outside it. A candidate that doesn't
 * exist (`realpathSync` throws) yields `null`. Appending `path.sep` to
 * both sides makes the `startsWith` a true directory-boundary check —
 * `/a/roots` must not be considered contained by `/a/root`. A root that
 * already ends in the separator (a filesystem root such as `/` or
 * `C:\`) is normalized first so the boundary prefix doesn't become
 * `//` / `C:\\` and reject everything under it.
 */
function canonicalizeContainedPath(candidate: string, canonicalRoots: string[]): string | null {
    let realCandidate: string;
    try {
        realCandidate = fs.realpathSync(candidate);
    } catch {
        return null;
    }
    for (const realRoot of canonicalRoots) {
        if (realCandidate === realRoot) return realCandidate;
        const boundary = realRoot.endsWith(path.sep) ? realRoot : realRoot + path.sep;
        if ((realCandidate + path.sep).startsWith(boundary)) {
            return realCandidate;
        }
    }
    return null;
}

function isKnitFigurePath(relativePath: string): boolean {
    const parts = relativePath.split(/[\\/]+/).filter(Boolean);
    return parts.length >= 2 && parts[0] === 'figure';
}

function withImgAttribute(attrs: string, name: string, value: string): string {
    const attrPattern = new RegExp(
        `\\s${name}\\s*=\\s*(?:"[^"]*"|'[^']*'|[^\\s>]+)`,
        'i',
    );
    const withoutExisting = attrs.replace(attrPattern, '');
    const attr = ` ${name}="${value}"`;
    const selfClosing = withoutExisting.match(/\s*\/\s*$/);
    if (!selfClosing) return withoutExisting + attr;
    return withoutExisting.slice(0, selfClosing.index) + attr + selfClosing[0];
}

export function mimeForImageExtension(ext: string): string | null {
    switch (ext) {
        case '.png': return 'image/png';
        case '.jpg':
        case '.jpeg': return 'image/jpeg';
        case '.gif': return 'image/gif';
        case '.svg': return 'image/svg+xml';
        case '.webp': return 'image/webp';
        case '.bmp': return 'image/bmp';
        case '.ico': return 'image/x-icon';
        case '.avif': return 'image/avif';
        default: return null;
    }
}
