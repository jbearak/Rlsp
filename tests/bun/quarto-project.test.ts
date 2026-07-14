import { describe, expect, test } from 'bun:test';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import {
    findQuartoProjectRoot,
    resolveQuartoContext,
    type QuartoProjectDeps,
} from '../../editors/vscode/src/quarto/quarto-project';
import { isQuartoProjectMarkerFile } from '../../editors/vscode/src/quarto/quarto-project-fs';

function fakeExists(paths: readonly string[], seen: string[] = []): QuartoProjectDeps {
    const existing = new Set(paths.map((candidate) => path.resolve(candidate)));
    return {
        isProjectMarkerFile(candidate: string): boolean {
            seen.push(candidate);
            return existing.has(path.resolve(candidate));
        },
    };
}

describe('Quarto project discovery', () => {
    test('finds the nearest project marker at depth N', () => {
        const root = path.resolve('/tmp/raven-quarto/project');
        const nested = path.join(root, 'chapters', 'deep');
        const deps = fakeExists([path.join(root, '_quarto.yml')]);

        expect(findQuartoProjectRoot(nested, deps)).toBe(root);
        expect(resolveQuartoContext(path.join(nested, 'report.qmd'), deps)).toEqual({
            key: root,
            cwd: root,
            projectRoot: root,
        });
    });

    test('recognizes the yaml marker spelling', () => {
        const root = path.resolve('/tmp/raven-quarto/yaml-project');
        expect(
            findQuartoProjectRoot(
                path.join(root, 'nested'),
                fakeExists([path.join(root, '_quarto.yaml')]),
            ),
        ).toBe(root);
    });

    test('falls back to file key and parent cwd when no marker exists', () => {
        const file = path.resolve('/tmp/raven-quarto/standalone/report.qmd');
        expect(resolveQuartoContext(file, fakeExists([]))).toEqual({
            key: file,
            cwd: path.dirname(file),
            projectRoot: null,
        });
    });

    test('checks the filesystem root and stops there', () => {
        const seen: string[] = [];
        const filesystemRoot = path.parse(path.resolve('/tmp/raven-quarto/deep')).root;
        const deps = fakeExists([], seen);

        expect(findQuartoProjectRoot('/tmp/raven-quarto/deep', deps)).toBeNull();
        expect(seen).toContain(path.join(filesystemRoot, '_quarto.yml'));
        expect(seen).toContain(path.join(filesystemRoot, '_quarto.yaml'));
        expect(seen.length).toBeLessThan(30);
    });

    test('does not treat a directory named _quarto.yml as a project marker', () => {
        const root = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-quarto-marker-'));
        try {
            const nested = path.join(root, 'chapters');
            fs.mkdirSync(path.join(root, '_quarto.yml'));
            fs.mkdirSync(nested);

            expect(findQuartoProjectRoot(nested, {
                isProjectMarkerFile: (candidate) =>
                    candidate.startsWith(root)
                    && isQuartoProjectMarkerFile(candidate),
            })).toBeNull();
        } finally {
            fs.rmSync(root, { recursive: true, force: true });
        }
    });
});
