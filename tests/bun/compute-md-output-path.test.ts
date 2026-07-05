import { describe, test, expect, beforeAll } from 'bun:test';
import * as path from 'path';
import * as os from 'os';
import { computeMdOutputPath, computeHtmlOutputPath } from '../../editors/vscode/src/knit/knit-paths';
import { initSessionState, __resetSessionStateForTests } from '../../editors/vscode/src/knit/session-state';
import { computeWorkspaceHash, computeSourceHash } from '../../editors/vscode/src/knit/raven-knit-paths';

/**
 * After the temp-dir migration in 2026-05-23-knit-preview-export-design,
 * `computeMdOutputPath` and `computeHtmlOutputPath` no longer return a
 * sibling-of-source path — they delegate to `previewArtifactPaths` which
 * resolves a per-session temp location. The tests now assert structural
 * properties (basename derivation, sibling md/html relationship) against
 * the temp root rather than against `/tmp/`.
 */
beforeAll(() => {
    __resetSessionStateForTests();
    initSessionState({ sessionId: 'test-session', workspaceUri: 'file:///tmp/test-workspace' });
});

const SESSION_WORKSPACE_HASH = computeWorkspaceHash('file:///tmp/test-workspace');

function expectedPreviewDir(rmdAbsPath: string): string {
    const sourceHash = computeSourceHash(rmdAbsPath);
    return path.join(os.tmpdir(), 'raven-knit', SESSION_WORKSPACE_HASH, 'test-session', 'preview', sourceHash);
}

describe('computeMdOutputPath', () => {
    test('strips lowercase .rmd and appends .md', () => {
        const out = computeMdOutputPath('/tmp/foo.rmd');
        expect(out).toBe(path.join(expectedPreviewDir('/tmp/foo.rmd'), 'foo.md'));
    });

    test('strips title-cased .Rmd and appends .md', () => {
        const out = computeMdOutputPath('/tmp/foo.Rmd');
        expect(out).toBe(path.join(expectedPreviewDir('/tmp/foo.Rmd'), 'foo.md'));
    });

    test('strips uppercase .RMD and appends .md', () => {
        const out = computeMdOutputPath('/tmp/foo.RMD');
        expect(out).toBe(path.join(expectedPreviewDir('/tmp/foo.RMD'), 'foo.md'));
    });

    test('strips .Rmarkdown case variants and appends .md', () => {
        expect(path.basename(computeMdOutputPath('/tmp/foo.rmarkdown'))).toBe('foo.md');
        expect(path.basename(computeMdOutputPath('/tmp/foo.Rmarkdown'))).toBe('foo.md');
        expect(path.basename(computeMdOutputPath('/tmp/foo.RMARKDOWN'))).toBe('foo.md');
    });

    test('handles paths with multiple dots in the basename', () => {
        const out = computeMdOutputPath('/tmp/foo.bar.Rmd');
        expect(out).toBe(path.join(expectedPreviewDir('/tmp/foo.bar.Rmd'), 'foo.bar.md'));
    });

    test('preserves source-hash uniqueness across nested paths', () => {
        const a = computeMdOutputPath('/a/b/c/doc.Rmd');
        const b = computeMdOutputPath('/x/y/z/doc.Rmd');
        // Same basename, different source hashes -> different temp dirs.
        expect(a).not.toBe(b);
        expect(path.basename(a)).toBe('doc.md');
        expect(path.basename(b)).toBe('doc.md');
    });

    test('falls back to appending .md when extension is missing', () => {
        const out = computeMdOutputPath('/tmp/foo');
        expect(out).toBe(path.join(expectedPreviewDir('/tmp/foo'), 'foo.md'));
    });
});

describe('computeHtmlOutputPath', () => {
    test('strips .Rmd and appends .html', () => {
        const out = computeHtmlOutputPath('/tmp/foo.Rmd');
        expect(out).toBe(path.join(expectedPreviewDir('/tmp/foo.Rmd'), 'foo.html'));
    });

    test('strips .rmd / .RMD / .Rmarkdown case variants', () => {
        expect(path.basename(computeHtmlOutputPath('/tmp/foo.rmd'))).toBe('foo.html');
        expect(path.basename(computeHtmlOutputPath('/tmp/foo.RMD'))).toBe('foo.html');
        expect(path.basename(computeHtmlOutputPath('/tmp/foo.Rmarkdown'))).toBe('foo.html');
    });

    test('matches computeMdOutputPath structurally — sibling md/html in same dir', () => {
        const md = computeMdOutputPath('/tmp/foo.Rmd');
        const html = computeHtmlOutputPath('/tmp/foo.Rmd');
        expect(path.dirname(md)).toBe(path.dirname(html));
        expect(md.replace(/\.md$/, '.html')).toBe(html);
    });
});
