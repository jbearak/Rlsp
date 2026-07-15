/**
 * Unit tests for `inlineLocalImagesAsDataUrls`. The function is the
 * workaround for the nested-iframe subresource issue in the Knit
 * Output panel: VS Code's webview resource handler does not intercept
 * subresource fetches from a nested `<iframe srcdoc>`, so the
 * webview-resource URL the `<base>` resolves an `<img src>` to
 * escapes the protocol handler and fails with a real DNS lookup.
 * Inlining the image bytes as `data:` URLs sidesteps the handler.
 *
 * The tests cover what the function MUST and MUST NOT rewrite, so a
 * future refactor can re-implement it freely (regex → parser, etc.)
 * as long as the contract holds.
 */
import { describe, test, expect } from 'bun:test';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

import { inlineLocalImagesAsDataUrls } from '../../editors/vscode/src/knit/inline-images';

function withTempDir<T>(fn: (dir: string) => T): T {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-inline-img-'));
    try {
        return fn(dir);
    } finally {
        try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* noop */ }
    }
}

// Smallest valid 1×1 transparent PNG.
const TINY_PNG = Buffer.from(
    '89504e470d0a1a0a0000000d4948445200000001000000010806000000' +
    '1f15c4890000000d49444154789c63000100000005000174ec61e30000' +
    '0000049454e44ae426082',
    'hex',
);

const ON_WINDOWS = process.platform === 'win32';

// Whether this host can create symlinks (Windows CI often cannot).
// Probed once so the symlink-escape test is an explicit, visible skip
// rather than a test that silently passes without asserting anything.
const SYMLINKS_SUPPORTED = (() => {
    try {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-symcap-'));
        try {
            fs.writeFileSync(path.join(dir, 'target'), 'x');
            fs.symlinkSync(path.join(dir, 'target'), path.join(dir, 'link'));
            return true;
        } finally {
            fs.rmSync(dir, { recursive: true, force: true });
        }
    } catch {
        return false;
    }
})();

describe('inlineLocalImagesAsDataUrls', () => {
    test('replaces a relative <img src> with a data: URL', () => {
        withTempDir((dir) => {
            const figDir = path.join(dir, 'figure');
            fs.mkdirSync(figDir, { recursive: true });
            fs.writeFileSync(path.join(figDir, 'plot-1.png'), TINY_PNG);

            const html = '<p><img src="figure/plot-1.png" alt="x" data-src="figure/plot-1.png"></p>';
            const out = inlineLocalImagesAsDataUrls(html, dir);

            expect(out).toContain('src="data:image/png;base64,');
            // The unmodified `data-src` attribute and the `alt`
            // attribute MUST survive — only `src` is rewritten.
            expect(out).toContain('alt="x"');
            expect(out).toContain('data-src="figure/plot-1.png"');
        });
    });

    test('leaves absolute http/https URLs alone', () => {
        const html = '<img src="https://example.com/x.png">';
        expect(inlineLocalImagesAsDataUrls(html, '/no/such/dir')).toBe(html);
    });

    test('leaves data: URLs alone', () => {
        const html = '<img src="data:image/png;base64,iVBORw0KGgo=">';
        expect(inlineLocalImagesAsDataUrls(html, '/no/such/dir')).toBe(html);
    });

    test('leaves vscode-webview / file URLs alone', () => {
        const html1 = '<img src="vscode-webview://abc/x.png">';
        const html2 = '<img src="file:///etc/hosts">';
        expect(inlineLocalImagesAsDataUrls(html1, '/tmp')).toBe(html1);
        expect(inlineLocalImagesAsDataUrls(html2, '/tmp')).toBe(html2);
    });

    test('leaves protocol-relative URLs alone', () => {
        const html = '<img src="//cdn.example/x.png">';
        expect(inlineLocalImagesAsDataUrls(html, '/tmp')).toBe(html);
    });

    test('leaves absolute filesystem paths outside every allowed root alone', () => {
        const html = '<img src="/usr/share/icons/x.png">';
        expect(inlineLocalImagesAsDataUrls(html, '/tmp')).toBe(html);
    });

    test('rejects path traversal — does NOT read files outside the doc dir', () => {
        withTempDir((dir) => {
            const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-outside-'));
            try {
                const secret = path.join(outsideDir, 'secret.png');
                fs.writeFileSync(secret, TINY_PNG);
                const innerDir = path.join(dir, 'inner');
                fs.mkdirSync(innerDir);

                // Resolves to outside the doc dir via `..` walks
                const html = `<img src="../../${path.relative(path.dirname(dir), secret)}">`;
                const out = inlineLocalImagesAsDataUrls(html, innerDir);
                // The src should remain its original (untrusted)
                // value — NOT be replaced with the secret file's
                // base64.
                expect(out).toContain('<img src="../../');
                expect(out).not.toContain('data:image/png;base64,');
            } finally {
                try { fs.rmSync(outsideDir, { recursive: true, force: true }); } catch { /* noop */ }
            }
        });
    });

    test('leaves missing files alone (no throw, src untouched)', () => {
        withTempDir((dir) => {
            const html = '<img src="figure/does-not-exist.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toBe(html);
        });
    });

    test('leaves unknown extensions alone', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'mystery.xyz'), TINY_PNG);
            const html = '<img src="mystery.xyz">';
            expect(inlineLocalImagesAsDataUrls(html, dir)).toBe(html);
        });
    });

    test('uses image/svg+xml for .svg', () => {
        withTempDir((dir) => {
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'icon.svg'), svg);
            const html = '<img src="icon.svg">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/svg+xml;base64,');
        });
    });

    test('marks local figure SVGs for panel-side inline theming when requested', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'figure', 'plot-1.svg'), svg);
            const html = '<p><img src="figure/plot-1.svg" alt="plot"></p>';

            const out = inlineLocalImagesAsDataUrls(html, dir, undefined, {
                markSvgPlots: true,
            });

            expect(out).toContain('src="data:image/svg+xml;base64,');
            expect(out).toContain('data-raven-plot-svg="true"');
            expect(out).toContain('alt="plot"');
        });
    });

    test('does not mark non-figure SVGs as themeable plots', () => {
        withTempDir((dir) => {
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'logo.svg'), svg);
            const html = '<img src="logo.svg" alt="logo">';

            const out = inlineLocalImagesAsDataUrls(html, dir, undefined, {
                markSvgPlots: true,
            });

            expect(out).toContain('src="data:image/svg+xml;base64,');
            expect(out).not.toContain('data-raven-plot-svg');
        });
    });

    test('does not mark paths that only appear to be under figure before normalization', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'logo.svg'), svg);
            const html = '<img src="figure/../logo.svg" alt="logo">';

            const out = inlineLocalImagesAsDataUrls(html, dir, undefined, {
                markSvgPlots: true,
            });

            expect(out).toContain('src="data:image/svg+xml;base64,');
            expect(out).not.toContain('data-raven-plot-svg');
        });
    });

    test('handles multiple <img> tags in the same document', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'a.png'), TINY_PNG);
            fs.writeFileSync(path.join(dir, 'figure', 'b.png'), TINY_PNG);
            const html =
                '<img src="figure/a.png" alt="A">' +
                '<p>text</p>' +
                '<img src="figure/b.png" alt="B">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            const matches = out.match(/data:image\/png;base64,/g) ?? [];
            expect(matches.length).toBe(2);
            expect(out).toContain('alt="A"');
            expect(out).toContain('alt="B"');
        });
    });

    test('leaves <img> with no src alone', () => {
        const html = '<img alt="no-src">';
        expect(inlineLocalImagesAsDataUrls(html, '/tmp')).toBe(html);
    });

    test('trims a leading ./ before resolving', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'p.png'), TINY_PNG);
            const html = '<img src="./figure/p.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('inlines through a ?query cache-buster suffix and drops the query', () => {
        // htmlwidgets and similar markdown renderers append a
        // version-style query to defeat HTTP caching. The suffix must be
        // split off the path (else the MIME lookup on `.png?v=1` fails
        // and the image breaks) AND dropped from the emitted data URL: a
        // `?` in the base64 data portion is invalid and would fail the
        // browser's forgiving-base64 decode.
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'plot.png'), TINY_PNG);
            const html = '<img src="figure/plot.png?v=1">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
            expect(out).not.toContain('?v=1');
        });
    });

    test('preserves a #fragment suffix on the rewritten data URL', () => {
        // `<img src="diagram.svg#layer-1">` is a real SVG view
        // identifier — browsers honor fragments on SVG `img`
        // sources to scroll to a named `<view>` element. The
        // fragment MUST survive the inline rewrite or the
        // panel's rendering of the image will differ from the
        // standalone HTML opened in a browser.
        withTempDir((dir) => {
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">' +
                '<view id="layer-1" viewBox="0 0 1 1"/></svg>';
            fs.writeFileSync(path.join(dir, 'diagram.svg'), svg);
            const html = '<img src="diagram.svg#layer-1">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/svg+xml;base64,');
            expect(out).toMatch(/src="data:image\/svg\+xml;base64,[^"]+#layer-1"/);
        });
    });

    // --- issue #627: existing workspace images via include_graphics() ---

    test('inlines an absolute path inside an additional allowed root', () => {
        withTempDir((workspace) => {
            const docDir = path.join(workspace, 'docs', '.raven_output');
            const imgDir = path.join(workspace, 'images');
            fs.mkdirSync(docDir, { recursive: true });
            fs.mkdirSync(imgDir, { recursive: true });
            const abs = path.join(imgDir, 'existing.png');
            fs.writeFileSync(abs, TINY_PNG);

            const html = `<img src="${abs}">`;
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                additionalRoots: [workspace],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('inlines a relative path that traverses out of docDir but stays in a root', () => {
        withTempDir((workspace) => {
            const docDir = path.join(workspace, 'docs', '.raven_output');
            const imgDir = path.join(workspace, 'images');
            fs.mkdirSync(docDir, { recursive: true });
            fs.mkdirSync(imgDir, { recursive: true });
            fs.writeFileSync(path.join(imgDir, 'existing.png'), TINY_PNG);

            // From docDir up to workspace, then into images/.
            const rel = path.relative(docDir, path.join(imgDir, 'existing.png'));
            const html = `<img src="${rel}">`;
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                additionalRoots: [workspace],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('resolves a relative author path against an extra resolveBase (root.dir)', () => {
        withTempDir((project) => {
            // include_graphics("images/logo.png") is relative to the knit
            // root.dir (project), NOT the preview output dir (docDir).
            const docDir = path.join(project, 'reports', '.raven_output');
            const imgDir = path.join(project, 'images');
            fs.mkdirSync(docDir, { recursive: true });
            fs.mkdirSync(imgDir, { recursive: true });
            fs.writeFileSync(path.join(imgDir, 'logo.png'), TINY_PNG);

            const html = '<img src="images/logo.png">';
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                resolveBases: [project],
                additionalRoots: [project],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('a relative author path is not inlined without a matching resolveBase', () => {
        withTempDir((project) => {
            const docDir = path.join(project, 'reports', '.raven_output');
            const imgDir = path.join(project, 'images');
            fs.mkdirSync(docDir, { recursive: true });
            fs.mkdirSync(imgDir, { recursive: true });
            fs.writeFileSync(path.join(imgDir, 'logo.png'), TINY_PNG);

            // project is an allowed root, but with docDir as the only
            // resolution base `images/logo.png` resolves to
            // docDir/images/logo.png (missing) — so it stays broken.
            const html = '<img src="images/logo.png">';
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                additionalRoots: [project],
            });
            expect(out).toBe(html);
        });
    });

    test('generated figure still resolves against docDir when a resolveBase is set', () => {
        withTempDir((project) => {
            // The docDir-relative plot must not regress now that an extra
            // base (root.dir) is also tried.
            const docDir = path.join(project, 'reports', '.raven_output');
            fs.mkdirSync(path.join(docDir, 'figure'), { recursive: true });
            fs.writeFileSync(path.join(docDir, 'figure', 'plot-1.png'), TINY_PNG);

            const html = '<img src="figure/plot-1.png">';
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                markSvgPlots: true,
                resolveBases: [project],
                additionalRoots: [project],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('treats a resolveBase as an allowed root without a duplicate additionalRoots entry', () => {
        withTempDir((project) => {
            const docDir = path.join(project, 'reports', '.raven_output');
            const imgDir = path.join(project, 'images');
            fs.mkdirSync(docDir, { recursive: true });
            fs.mkdirSync(imgDir, { recursive: true });
            fs.writeFileSync(path.join(imgDir, 'logo.png'), TINY_PNG);
            // Only resolveBases is given (no additionalRoots): the base
            // must still be allowed for containment, or resolution would
            // succeed but every result be rejected.
            const html = '<img src="images/logo.png">';
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                resolveBases: [project],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('re-escapes a decoded #fragment so it cannot break out of the src attribute', () => {
        withTempDir((dir) => {
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'x.svg'), svg);
            // The source fragment contains an entity-encoded quote; after
            // entity-decoding it would be a raw `"` that must be
            // re-escaped when spliced back into src="…".
            const html = '<img src="x.svg#a&quot;b">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/svg+xml;base64,');
            // The quote rides along re-escaped, not raw.
            expect(out).toContain('#a&quot;b');
            // Exactly one src attribute — the tag wasn't corrupted into two.
            expect(out.match(/\bsrc=/g)?.length).toBe(1);
        });
    });

    test.skipIf(!SYMLINKS_SUPPORTED)('labels a symlinked image by its target type, not the link name', () => {
        withTempDir((dir) => {
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'real.svg'), svg);
            // `logo.png` is a symlink to an SVG; the data URL MIME must
            // match the bytes actually read (svg), not the .png link name.
            fs.symlinkSync(path.join(dir, 'real.svg'), path.join(dir, 'logo.png'));
            const html = '<img src="logo.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/svg+xml;base64,');
            expect(out).not.toContain('data:image/png;base64,');
        });
    });

    test('blocks an absolute path outside every allowed root', () => {
        withTempDir((workspace) => {
            const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-outside-'));
            try {
                const docDir = path.join(workspace, 'docs', '.raven_output');
                fs.mkdirSync(docDir, { recursive: true });
                const secret = path.join(outside, 'secret.png');
                fs.writeFileSync(secret, TINY_PNG);

                const html = `<img src="${secret}">`;
                const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                    additionalRoots: [workspace],
                });
                expect(out).toBe(html);
                expect(out).not.toContain('data:image/png;base64,');
            } finally {
                try { fs.rmSync(outside, { recursive: true, force: true }); } catch { /* noop */ }
            }
        });
    });

    test.skipIf(!SYMLINKS_SUPPORTED)('does not follow a symlink out of an allowed root', () => {
        withTempDir((workspace) => {
            const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-outside-'));
            try {
                const secret = path.join(outside, 'secret.png');
                fs.writeFileSync(secret, TINY_PNG);
                // A symlink that lives inside the workspace but points out.
                const link = path.join(workspace, 'leak.png');
                fs.symlinkSync(secret, link);
                const html = '<img src="leak.png">';
                const out = inlineLocalImagesAsDataUrls(html, workspace);
                expect(out).toBe(html);
                expect(out).not.toContain('data:image/png;base64,');
            } finally {
                try { fs.rmSync(outside, { recursive: true, force: true }); } catch { /* noop */ }
            }
        });
    });

    test('handles spaces and non-ASCII characters in image names', () => {
        withTempDir((workspace) => {
            const docDir = path.join(workspace, '.raven_output');
            const imgDir = path.join(workspace, 'images');
            fs.mkdirSync(docDir, { recursive: true });
            fs.mkdirSync(imgDir, { recursive: true });
            const name = 'my café pläot.png';
            fs.writeFileSync(path.join(imgDir, name), TINY_PNG);

            const rel = path.relative(docDir, path.join(imgDir, name));
            const html = `<img src="${rel}">`;
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                additionalRoots: [workspace],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('skips a non-existent additional root without throwing', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'p.png'), TINY_PNG);
            const html = '<img src="figure/p.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir, undefined, {
                additionalRoots: [path.join(dir, 'does-not-exist')],
            });
            // docDir is still an allowed root, so the figure image inlines.
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    // --- sol-review follow-ups (#627) ---

    test('rewrites src, not data-src, even when data-src comes first', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'real.png'), TINY_PNG);
            // data-src precedes the real src; a bare \bsrc matcher would
            // wrongly select the `src` inside `data-src`.
            const html = '<img data-src="figure/thumb.png" src="figure/real.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
            // The data-src attribute must survive untouched.
            expect(out).toContain('data-src="figure/thumb.png"');
        });
    });

    test('resolves a percent-encoded non-ASCII path (markdown-it output)', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'images'));
            fs.writeFileSync(path.join(dir, 'images', 'café.png'), TINY_PNG);
            // markdown-it percent-encodes the path in the rendered HTML.
            const html = '<img src="images/caf%C3%A9.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('resolves a percent-encoded space in the path', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'a b.png'), TINY_PNG);
            const html = '<img src="a%20b.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('resolves an HTML-entity-escaped ampersand in the path', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'a&b.png'), TINY_PNG);
            const html = '<img src="a&amp;b.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('docDir wins across BOTH spellings before a resolveBase is tried', () => {
        withTempDir((project) => {
            const docDir = path.join(project, '.raven_output');
            fs.mkdirSync(docDir, { recursive: true });
            // docDir has only the LITERAL percent name; the resolveBase
            // has the DECODED name. docDir must win (documented
            // docDir-first precedence), so the literal file is inlined.
            const docBytes = TINY_PNG;
            const baseBytes = Buffer.from('distinct-not-really-png');
            fs.writeFileSync(path.join(docDir, 'a%20b.png'), docBytes);
            fs.writeFileSync(path.join(project, 'a b.png'), baseBytes);
            const html = '<img src="a%20b.png">';
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                resolveBases: [project],
            });
            expect(out).toContain(`data:image/png;base64,${docBytes.toString('base64')}`);
            expect(out).not.toContain(baseBytes.toString('base64'));
        });
    });

    test('still resolves a filename that literally contains a percent', () => {
        withTempDir((dir) => {
            // Raw passthrough <img> whose file is literally named with `%`
            // and whose decoded form (`a b.png`) does NOT exist. The
            // decoded candidate is tried first; when it misses, the
            // literal fallback resolves the real file.
            fs.writeFileSync(path.join(dir, 'a%20b.png'), TINY_PNG);
            const html = '<img src="a%20b.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test.skipIf(ON_WINDOWS)('leaves a one-letter URL scheme alone (not a drive path off Windows)', () => {
        // Off Windows, `x:/logo.png` is a custom URL scheme, not a drive
        // path; it must be classified as a URL and pass through. To prove
        // it isn't being resolved as a filesystem path, place a file
        // where a path interpretation would land and assert it is NOT
        // inlined.
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'x:'), { recursive: true });
            fs.writeFileSync(path.join(dir, 'x:', 'logo.png'), TINY_PNG);
            const html = '<img src="x:/logo.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toBe(html);
            expect(out).not.toContain('data:');
        });
    });

    test('logs a diagnostic when a would-be-inlinable image cannot be resolved', () => {
        withTempDir((dir) => {
            const lines: string[] = [];
            const sink = { appendLine: (l: string) => lines.push(l) };
            const html = '<img src="images/missing.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir, sink);
            expect(out).toBe(html);
            expect(lines.some((l) => l.includes('not inlining image') && l.includes('missing.png')))
                .toBe(true);
        });
    });

    test('the diagnostic names every base tried, including the root.dir base', () => {
        withTempDir((project) => {
            const docDir = path.join(project, 'reports', '.raven_output');
            fs.mkdirSync(docDir, { recursive: true });
            const lines: string[] = [];
            const sink = { appendLine: (l: string) => lines.push(l) };
            // Missing under both docDir and the root.dir base (project).
            const out = inlineLocalImagesAsDataUrls('<img src="images/missing.png">', docDir, sink, {
                resolveBases: [project],
                additionalRoots: [project],
            });
            expect(out).toContain('<img src="images/missing.png">');
            const line = lines.find((l) => l.includes('not inlining image'));
            expect(line).toBeDefined();
            // Both the docDir attempt and the project (root.dir) attempt
            // are reported so the author sees the real paths to fix.
            expect(line).toContain(path.join(docDir, 'images', 'missing.png'));
            expect(line).toContain(path.join(project, 'images', 'missing.png'));
        });
    });

    test('does not log for images that inline successfully', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'p.png'), TINY_PNG);
            const lines: string[] = [];
            const sink = { appendLine: (l: string) => lines.push(l) };
            const out = inlineLocalImagesAsDataUrls('<img src="figure/p.png">', dir, sink);
            expect(out).toContain('src="data:image/png;base64,');
            expect(lines).toEqual([]);
        });
    });

    test('drops the ?query but keeps the #fragment when both are present', () => {
        // `?v=1#frag`: the fragment is a real URL component (selects an
        // SVG `<view>`) and must survive; the query is dropped because a
        // `?` in the base64 data portion is an invalid code point that
        // fails forgiving-base64 decoding and breaks the image.
        withTempDir((dir) => {
            const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>';
            fs.writeFileSync(path.join(dir, 'icon.svg'), svg);
            const html = '<img src="icon.svg?v=1#frag">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/svg+xml;base64,');
            expect(out).toMatch(/src="data:image\/svg\+xml;base64,[^"?]+#frag"/);
            expect(out).not.toContain('?v=1');
        });
    });

    test('inlines the src, not a src= substring inside another attribute', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'real.png'), TINY_PNG);
            // The alt value literally contains `src='thumb.png'`; a
            // quote-blind matcher would rewrite that instead of the real
            // src and mangle the alt text.
            const html = `<img alt="see src='thumb.png'" src="real.png">`;
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
            // The alt attribute must be preserved verbatim.
            expect(out).toContain(`alt="see src='thumb.png'"`);
        });
    });

    test('allows a root that is a filesystem root (trailing-separator boundary)', () => {
        // A workspace folder that is the filesystem root ends in the
        // path separator; the boundary check must not build a `//`
        // prefix that rejects everything under it.
        withTempDir((dir) => {
            const docDir = path.join(dir, '.raven_output');
            fs.mkdirSync(docDir, { recursive: true });
            // Image lives outside docDir but under the "/" root we allow.
            const img = path.join(dir, 'pic.png');
            fs.writeFileSync(img, TINY_PNG);
            const html = `<img src="${img}">`;
            const out = inlineLocalImagesAsDataUrls(html, docDir, undefined, {
                additionalRoots: [path.parse(dir).root],
            });
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('replaces the real src even when an earlier attr holds identical src= text', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'logo.png'), TINY_PNG);
            // The alt value is textually identical to the real src
            // attribute. A `String.replace(matchText, …)` would rewrite
            // the copy inside alt (first textual occurrence); positional
            // splicing must target the real attribute instead.
            const html = `<img alt="src='logo.png'" src="logo.png">`;
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
            // The alt attribute must be preserved verbatim.
            expect(out).toContain(`alt="src='logo.png'"`);
        });
    });

    test('inlines when an attribute value contains a literal > character', () => {
        withTempDir((dir) => {
            fs.mkdirSync(path.join(dir, 'figure'));
            fs.writeFileSync(path.join(dir, 'figure', 'plot.png'), TINY_PNG);
            // A literal `>` inside alt must not terminate the <img> tag
            // before the src attribute is seen.
            const html = '<img alt="a > b" src="figure/plot.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
            expect(out).toContain('alt="a > b"');
        });
    });

    test('decodes a decimal numeric HTML entity in the path', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'a&b.png'), TINY_PNG);
            const html = '<img src="a&#38;b.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('decodes a hex numeric HTML entity in the path', () => {
        withTempDir((dir) => {
            fs.writeFileSync(path.join(dir, 'a&b.png'), TINY_PNG);
            const html = '<img src="a&#x26;b.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain('src="data:image/png;base64,');
        });
    });

    test('prefers the decoded filename over a literal percent-encoded twin', () => {
        // Both `a b.png` and a literal `a%20b.png` exist. A browser
        // percent-decodes `src="a%20b.png"` to `a b.png`, so the inliner
        // must inline `a b.png`, not the literal twin.
        withTempDir((dir) => {
            const decodedBytes = TINY_PNG;
            const literalBytes = Buffer.from('not-a-real-png-but-distinct');
            fs.writeFileSync(path.join(dir, 'a b.png'), decodedBytes);
            fs.writeFileSync(path.join(dir, 'a%20b.png'), literalBytes);
            const html = '<img src="a%20b.png">';
            const out = inlineLocalImagesAsDataUrls(html, dir);
            expect(out).toContain(`src="data:image/png;base64,${decodedBytes.toString('base64')}"`);
            expect(out).not.toContain(literalBytes.toString('base64'));
        });
    });
});
