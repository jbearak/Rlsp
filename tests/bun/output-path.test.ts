import { describe, test, expect } from 'bun:test';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { parseRenderedOutputPath } from '../../editors/vscode/src/knit/output-path';

const FIXTURE_DIR = path.resolve(__dirname, '..', 'fixtures', 'rmarkdown-stdout');
const readFixture = (name: string): string =>
    fs.readFileSync(path.join(FIXTURE_DIR, name), 'utf8');

describe('parseRenderedOutputPath', () => {
    test('extracts a single output path', () => {
        const stdout = [
            "processing file: foo.Rmd",
            "  |......                                          |  10%  ordinary text without R code",
            "",
            "Output created: foo.html",
            "",
        ].join('\n');
        const result = parseRenderedOutputPath(stdout);
        expect(result.paths).toEqual(['foo.html']);
    });

    test('extracts an absolute path', () => {
        const stdout = "Output created: /Users/me/proj/foo.html\n";
        expect(parseRenderedOutputPath(stdout).paths).toEqual(['/Users/me/proj/foo.html']);
    });

    test('extracts a Windows drive-letter absolute path', () => {
        const stdout = "Output created: C:\\Users\\me\\proj\\foo.html\r\n";
        expect(parseRenderedOutputPath(stdout).paths)
            .toEqual(['C:\\Users\\me\\proj\\foo.html']);
    });

    test('returns empty when message is absent (quiet mode)', () => {
        const stdout = "rendering...\n";
        expect(parseRenderedOutputPath(stdout).paths).toEqual([]);
    });

    test('returns multiple paths for "all" output', () => {
        const stdout = [
            "Output created: foo.html",
            "more progress",
            "Output created: foo.pdf",
            "Output created: foo.docx",
        ].join('\n');
        expect(parseRenderedOutputPath(stdout).paths).toEqual([
            'foo.html', 'foo.pdf', 'foo.docx',
        ]);
    });

    test('trims trailing whitespace from the path', () => {
        const stdout = "Output created: foo.html   \n";
        expect(parseRenderedOutputPath(stdout).paths).toEqual(['foo.html']);
    });

    test('handles CRLF line endings', () => {
        const stdout = "Output created: foo.html\r\n";
        expect(parseRenderedOutputPath(stdout).paths).toEqual(['foo.html']);
    });

    test('matches paths with spaces (rmarkdown does not quote them)', () => {
        const stdout = "Output created: /Users/me/My Project/foo.html\n";
        expect(parseRenderedOutputPath(stdout).paths).toEqual([
            '/Users/me/My Project/foo.html',
        ]);
    });

    test('ignores leading whitespace on the message line', () => {
        const stdout = "    Output created: foo.html\n";
        expect(parseRenderedOutputPath(stdout).paths).toEqual(['foo.html']);
    });

    test('extracts the path from a realistic html_document run', () => {
        expect(parseRenderedOutputPath(readFixture('html_document.txt')).paths)
            .toEqual(['example.html']);
    });

    test('extracts an absolute path from a realistic pdf_document run', () => {
        expect(parseRenderedOutputPath(readFixture('pdf_document.txt')).paths)
            .toEqual(['/Users/me/proj/example.pdf']);
    });

    test('extracts all three paths from an output_format = "all" run', () => {
        expect(parseRenderedOutputPath(readFixture('all.txt')).paths)
            .toEqual(['example.html', 'example.pdf', 'example.docx']);
    });

    test('parses the reported knit root directory', () => {
        const stdout = [
            'Output created: /proj/reports/.raven_output/report.md',
            'Raven-knit-root: /proj',
        ].join('\n');
        const result = parseRenderedOutputPath(stdout);
        expect(result.paths).toEqual(['/proj/reports/.raven_output/report.md']);
        expect(result.rootDir).toBe('/proj');
    });

    test('rootDir is null when the root line is absent', () => {
        expect(parseRenderedOutputPath('Output created: foo.md\n').rootDir).toBeNull();
    });

    test('the last reported root wins (matches R\'s last write)', () => {
        const stdout = [
            'Raven-knit-root: /first',
            'Output created: foo.md',
            'Raven-knit-root: /proj/override',
        ].join('\n');
        expect(parseRenderedOutputPath(stdout).rootDir).toBe('/proj/override');
    });

    test('parses a root path containing spaces', () => {
        const stdout = 'Raven-knit-root: /Users/me/My Project\n';
        expect(parseRenderedOutputPath(stdout).rootDir).toBe('/Users/me/My Project');
    });
});
