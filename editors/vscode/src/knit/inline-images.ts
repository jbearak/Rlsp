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
 *     directory); absolute paths resolve as-is. Either way the result
 *     must land inside an allowed root.
 *   - Unknown extensions (anything not in
 *     `mimeForImageExtension`) are passed through; we don't read
 *     arbitrary file types off disk in case a future markdown
 *     pipeline starts producing `<img>` to non-image resources.
 *
 * Tests live in `tests/bun/inline-local-images.test.ts`.
 */
import * as fs from 'fs';
import * as path from 'path';

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
}

export function inlineLocalImagesAsDataUrls(
    html: string,
    docDir: string,
    output?: InlineImagesOutputSink,
    options: InlineImagesOptions = {},
): string {
    const allowedRoots = [docDir, ...(options.additionalRoots ?? [])];
    return html.replace(/<img\b([^>]*)>/gi, (match, attrs: string) => {
        const srcMatch = attrs.match(/\bsrc\s*=\s*"([^"]*)"/i)
            ?? attrs.match(/\bsrc\s*=\s*'([^']*)'/i);
        if (!srcMatch) return match;
        const src = srcMatch[1];

        // Already an absolute URL (any scheme, e.g. `https:`,
        // `data:`, `vscode-webview:`, `file:`) — pass through. A
        // Windows drive path (`C:\…`) also matches the single-letter
        // "scheme" shape, so exclude it: it's an absolute filesystem
        // path we may still want to inline.
        const isWindowsDrivePath = /^[a-z]:[\\/]/i.test(src);
        if (!isWindowsDrivePath && /^(?:[a-z][a-z0-9+\-.]*:)/i.test(src)) return match;
        // Protocol-relative URL.
        if (src.startsWith('//')) return match;

        // Split src into the path portion and any trailing
        // `?query` / `#fragment` suffix. htmlwidgets and similar
        // markdown renderers sometimes emit cache-busters
        // (`figure/plot.png?v=1`) and SVG view fragments
        // (`diagram.svg#layer-1`). If we feed the whole src to
        // `path.resolve` / `path.extname`, the suffix lands inside
        // the filename and:
        //   - the file-resolution `path.resolve(docDir, 'foo.png?v=1')`
        //     points at a non-existent file (silent failure: the
        //     src is returned unchanged and the broken-image icon
        //     still surfaces in the nested iframe);
        //   - `path.extname` returns `.png?v=1`, which doesn't map
        //     to a MIME and the inline pass bails out.
        //
        // Splitting on the first `?` or `#` recovers the original
        // file path. The fragment portion is re-attached to the
        // emitted data URL because SVG fragment identifiers can
        // navigate to a specific `<view>` element when used in
        // `<img src="x.svg#viewname">`. The query portion is
        // meaningless on a data URL (the URL itself IS the
        // content, so there's nothing to cache-bust) but is also
        // preserved for round-trip honesty — the cost is just a
        // few extra bytes in the rewritten HTML.
        const suffixStart = src.search(/[?#]/);
        const srcPath = suffixStart >= 0 ? src.slice(0, suffixStart) : src;
        const srcSuffix = suffixStart >= 0 ? src.slice(suffixStart) : '';

        // Absolute paths resolve as-is; relative paths resolve against
        // the rendered document's directory (`docDir`). The leading
        // `./` strip doesn't change semantics — it just keeps
        // `normalizedRelative` (used for the figure-plot marker) clean.
        const resolved = path.isAbsolute(srcPath)
            ? path.resolve(srcPath)
            : path.resolve(docDir, srcPath.replace(/^\.\//, ''));

        const ext = path.extname(resolved).toLowerCase();
        const mime = mimeForImageExtension(ext);
        if (!mime) return match;

        // Canonicalize the candidate and each allowed root with
        // `realpath` (so a symlink can't escape a root) and require the
        // file to be contained by at least one. Fails closed: a missing
        // file, a symlink target outside every root, or a path that
        // simply lives elsewhere all leave the `src` untouched.
        const realResolved = canonicalizeContainedPath(resolved, allowedRoots);
        if (realResolved === null) return match;

        // The figure-plot marker keys off the path relative to `docDir`
        // (its logical, pre-realpath form), so it only ever matches the
        // preview-generated `docDir/figure/*` tree, not workspace images.
        const normalizedRelative = path.relative(path.resolve(docDir), resolved);

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

        const dataUrl = `data:${mime};base64,${bytes.toString('base64')}${srcSuffix}`;
        const rewrittenAttrs = attrs.replace(srcMatch[0], `src="${dataUrl}"`);
        const finalAttrs = options.markSvgPlots && ext === '.svg' && isKnitFigurePath(normalizedRelative)
            ? withImgAttribute(rewrittenAttrs, 'data-raven-plot-svg', 'true')
            : rewrittenAttrs;
        return `<img${finalAttrs}>`;
    });
}

/**
 * Canonicalize `candidate` with `realpath` and return the real path
 * only if it is contained by (or equal to) at least one canonicalized
 * `root`; otherwise `null`.
 *
 * Both sides are `realpath`-resolved so a symlink living inside a root
 * cannot smuggle in a target outside it. A candidate that doesn't
 * exist (`realpathSync` throws) yields `null`, as does a root that
 * doesn't exist (skipped). The `+ path.sep` on both sides makes the
 * `startsWith` a true directory-boundary check — `/a/roots` must not
 * be considered contained by `/a/root`.
 */
function canonicalizeContainedPath(candidate: string, roots: string[]): string | null {
    let realCandidate: string;
    try {
        realCandidate = fs.realpathSync(candidate);
    } catch {
        return null;
    }
    for (const root of roots) {
        let realRoot: string;
        try {
            realRoot = fs.realpathSync(root);
        } catch {
            continue;
        }
        if (realCandidate === realRoot) return realCandidate;
        if ((realCandidate + path.sep).startsWith(realRoot + path.sep)) {
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
