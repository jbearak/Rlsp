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
        realpath: async (candidate) => path.resolve(candidate),
        async isProjectMarkerFile(candidate: string): Promise<boolean> {
            seen.push(candidate);
            return existing.has(path.resolve(candidate));
        },
    };
}

describe('Quarto project discovery', () => {
    test('finds the nearest project marker at depth N', async () => {
        const root = path.resolve('/tmp/raven-quarto/project');
        const nested = path.join(root, 'chapters', 'deep');
        const deps = fakeExists([path.join(root, '_quarto.yml')]);

        expect(await findQuartoProjectRoot(nested, deps)).toBe(root);
        expect(await resolveQuartoContext(path.join(nested, 'report.qmd'), deps)).toEqual({
            key: root,
            cwd: root,
            projectRoot: root,
        });
    });

    test('recognizes the yaml marker spelling', async () => {
        const root = path.resolve('/tmp/raven-quarto/yaml-project');
        expect(
            await findQuartoProjectRoot(
                path.join(root, 'nested'),
                fakeExists([path.join(root, '_quarto.yaml')]),
            ),
        ).toBe(root);
    });

    test('falls back to file key and parent cwd when no marker exists', async () => {
        const file = path.resolve('/tmp/raven-quarto/standalone/report.qmd');
        expect(await resolveQuartoContext(file, fakeExists([]))).toEqual({
            key: file,
            cwd: path.dirname(file),
            projectRoot: null,
        });
    });

    test('checks the filesystem root and stops there', async () => {
        const seen: string[] = [];
        const filesystemRoot = path.parse(path.resolve('/tmp/raven-quarto/deep')).root;
        const deps = fakeExists([], seen);

        expect(await findQuartoProjectRoot('/tmp/raven-quarto/deep', deps)).toBeNull();
        expect(seen).toContain(path.join(filesystemRoot, '_quarto.yml'));
        expect(seen).toContain(path.join(filesystemRoot, '_quarto.yaml'));
        expect(seen.length).toBeLessThan(30);
    });

    test('does not treat a directory named _quarto.yml as a project marker', async () => {
        const root = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-quarto-marker-'));
        try {
            const nested = path.join(root, 'chapters');
            fs.mkdirSync(path.join(root, '_quarto.yml'));
            fs.mkdirSync(nested);

            expect(await findQuartoProjectRoot(nested, {
                realpath: fs.promises.realpath,
                isProjectMarkerFile: async (candidate) =>
                    candidate.startsWith(root)
                    && await isQuartoProjectMarkerFile(candidate),
            })).toBeNull();
        } finally {
            fs.rmSync(root, { recursive: true, force: true });
        }
    });

    test('realpaths a source symlink before project classification', async () => {
        const base = fs.mkdtempSync(path.join(os.tmpdir(), 'raven-quarto-realpath-'));
        try {
            const project = path.join(base, 'project');
            const realSource = path.join(project, 'chapters', 'doc.qmd');
            const alias = path.join(base, 'outside.qmd');
            fs.mkdirSync(path.dirname(realSource), { recursive: true });
            fs.writeFileSync(path.join(project, '_quarto.yml'), 'project:\n  type: default\n');
            fs.writeFileSync(realSource, '# document\n');
            fs.symlinkSync(realSource, alias);
            const deps: QuartoProjectDeps = {
                realpath: fs.promises.realpath,
                isProjectMarkerFile: isQuartoProjectMarkerFile,
            };

            const realContext = await resolveQuartoContext(realSource, deps);
            const aliasContext = await resolveQuartoContext(alias, deps);
            const physicalProject = await fs.promises.realpath(project);

            expect(aliasContext).toEqual(realContext);
            expect(aliasContext.projectRoot).toBe(physicalProject);
            expect(aliasContext.key).toBe(physicalProject);
        } finally {
            fs.rmSync(base, { recursive: true, force: true });
        }
    });
});
