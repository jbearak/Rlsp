import { describe, expect, it } from 'bun:test';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { resolveQuartoRenderedOutputPath } from '../../editors/vscode/src/quarto/quarto-render-output';

describe('Quarto rendered output path resolution', () => {
    it('prefers an existing source-directory-relative project-subdir output', () => {
        const project = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-quarto-output-'));
        try {
            const source = path.join(project, 'chapters', 'doc.qmd');
            const output = path.join(project, 'chapters', 'rendered', 'doc.html');
            fs.mkdirSync(path.dirname(output), { recursive: true });
            fs.writeFileSync(source, '# doc');
            fs.writeFileSync(output, '<html></html>');

            expect(resolveQuartoRenderedOutputPath(
                'rendered/doc.html',
                source,
                project,
            )).toBe(output);
        } finally {
            fs.rmSync(project, { recursive: true, force: true });
        }
    });

    it('chooses a fresh cwd-relative website output over a stale source-relative file', () => {
        const source = '/project/chapters/doc.qmd';
        const sourceOutput = path.resolve('/project/chapters', 'site/doc.html');
        const cwdOutput = path.resolve('/project', 'site/doc.html');
        expect(resolveQuartoRenderedOutputPath(
            'site/doc.html',
            source,
            '/project',
            {
                exists: (candidate) => (
                    candidate === sourceOutput || candidate === cwdOutput
                ),
                mtimeMs: (candidate) => candidate === cwdOutput ? 200 : 100,
            },
        )).toBe(cwdOutput);
    });

    it('falls back gracefully to the source-relative path when neither exists', () => {
        const source = '/project/chapters/doc.qmd';
        expect(resolveQuartoRenderedOutputPath(
            'missing/doc.html',
            source,
            '/project',
            { exists: () => false, mtimeMs: () => 0 },
        )).toBe(path.resolve('/project/chapters', 'missing/doc.html'));
    });
});
