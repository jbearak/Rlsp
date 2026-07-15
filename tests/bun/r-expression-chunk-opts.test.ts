import { describe, it, expect } from 'bun:test';
import { buildKnitExpression } from '../../editors/vscode/src/knit/r-expression';
import type { ChunkOpts } from '../../editors/vscode/src/knit/output-options';

const noChunkOpts: ChunkOpts = {};

describe('buildKnitExpression with chunk opts', () => {
    it('emits assignments + opts_chunk$set for known chunk keys', () => {
        const expr = buildKnitExpression({
            filePath: '/p/foo.Rmd',
            outputPath: '/tmp/foo.md',
            format: 'html_document',
            knitRootDir: null,
            baseDir: '/tmp',
            figPath: 'figure/',
            chunkOpts: { fig_width: 5, fig_height: 4, dpi: 150, dev: 'png' },
        });
        // R-side variable assignments precede the opts_chunk$set call
        // (per the R-subprocess-safety invariant in CLAUDE.md). The
        // `.raven_*` prefix is the R idiom for "internal / hidden"
        // — a leading `_` would be a parse error.
        expect(expr).toContain('.raven_fig_width <- 5');
        expect(expr).toContain('.raven_fig_height <- 4');
        expect(expr).toContain('.raven_dpi <- 150L');
        expect(expr).toContain(".raven_dev <- 'png'");
        expect(expr).toContain('fig.width = .raven_fig_width');
        expect(expr).toContain('fig.height = .raven_fig_height');
        expect(expr).toContain('dpi = .raven_dpi');
        expect(expr).toContain('dev = .raven_dev');
    });

    it('emits opts_knit$set with base.dir + root.dir and the global fig.path', () => {
        const expr = buildKnitExpression({
            filePath: '/p/foo.Rmd',
            outputPath: '/tmp/foo.md',
            format: 'html_document',
            knitRootDir: '/p',
            baseDir: '/tmp/preview',
            figPath: 'figure/',
            chunkOpts: noChunkOpts,
        });
        expect(expr).toContain("base.dir = '/tmp/preview'");
        expect(expr).toContain("root.dir = '/p'");
        expect(expr).toContain("opts_chunk$set(fig.path = 'figure/')");
    });

    it('omits the YAML opts_chunk$set call when chunkOpts is empty', () => {
        const expr = buildKnitExpression({
            filePath: '/p/foo.Rmd',
            outputPath: '/tmp/foo.md',
            format: 'html_document',
            knitRootDir: null,
            baseDir: '/tmp',
            figPath: 'figure/',
            chunkOpts: noChunkOpts,
        });
        const optsCalls = expr.match(/opts_chunk\$set/g) ?? [];
        // Exactly one call — the fig.path one, never a YAML chunk-opts one.
        expect(optsCalls.length).toBe(1);
    });

    it('rejects dev values outside the allowlist', () => {
        expect(() =>
            buildKnitExpression({
                filePath: '/p/foo.Rmd',
                outputPath: '/tmp/foo.md',
                format: 'html_document',
                knitRootDir: null,
                baseDir: '/tmp',
                figPath: 'figure/',
                chunkOpts: { dev: "png'; system('rm -rf /')" } as ChunkOpts,
            }),
        ).toThrow();
    });

    it('uses getwd() when knitRootDir is null', () => {
        const expr = buildKnitExpression({
            filePath: '/p/foo.Rmd',
            outputPath: '/tmp/foo.md',
            format: 'html_document',
            knitRootDir: null,
            baseDir: '/tmp',
            figPath: 'figure/',
            chunkOpts: noChunkOpts,
        });
        expect(expr).toContain('root.dir = getwd()');
    });
});
