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

    it('uses an existing cwd-relative path, then source-relative fallback', () => {
        const source = '/project/chapters/doc.qmd';
        const cwdOutput = path.resolve('/project', 'site/doc.html');
        expect(resolveQuartoRenderedOutputPath(
            'site/doc.html',
            source,
            '/project',
            (candidate) => candidate === cwdOutput,
        )).toBe(cwdOutput);
        expect(resolveQuartoRenderedOutputPath(
            'missing/doc.html',
            source,
            '/project',
            () => false,
        )).toBe(path.resolve('/project/chapters', 'missing/doc.html'));
    });
});
